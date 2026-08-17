//! The Postgres boundary.
//!
//! Everything crossing out of this module is a rendered `String`. No
//! `postgres::Row`, no `Type`, no OIDs — the UI layer never learns which engine
//! it is talking to, which is the one concession made toward a second driver
//! later (see the spec, §4.1).
//!
//! **Results come back via the simple query protocol**, which returns every
//! value already formatted as text by the server. That removes the whole
//! per-type decoding layer a generic SQL client would otherwise need: numerics
//! keep their exact precision, `jsonb` arrives as JSON, geometry arrives as the
//! hex WKB the server would print, and unknown or extension types format
//! themselves correctly instead of falling through a match arm we forgot.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use postgres::{Client, NoTls, SimpleQueryMessage};

#[derive(Debug, PartialEq, Eq)]
pub struct ConnectionConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    /// Blank is valid and must never be warned about — cloud IAM auth issues a
    /// short-lived token as the password, or none at all.
    pub password: String,
}

impl ConnectionConfig {
    /// libpq key/value connection string.
    ///
    /// Values are single-quoted and escaped rather than interpolated bare, so
    /// that a username containing `@` or a password containing a space is
    /// passed through intact instead of truncating the string.
    pub fn connection_string(&self) -> String {
        let mut parts = vec![
            format!("host={}", quote(&self.host)),
            format!("port={}", self.port),
            format!("dbname={}", quote(&self.database)),
            format!("user={}", quote(&self.user)),
        ];
        if !self.password.is_empty() {
            parts.push(format!("password={}", quote(&self.password)));
        }
        parts.join(" ")
    }

    fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

fn quote(value: &str) -> String {
    let escaped = value.replace('\\', r"\\").replace('\'', r"\'");
    format!("'{escaped}'")
}

/// One column of a result set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: String,
}

/// A cell value, already formatted by the server. `None` is SQL NULL, which is
/// distinct from an empty string and must stay distinguishable in the grid.
pub type Cell = Option<String>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Cell>>,
    /// Total bytes of returned cell text. Shown in the status bar so the cost
    /// of a wide or geometry-heavy result is visible rather than mysterious.
    pub bytes: usize,
    pub elapsed: Duration,
    /// The command's server-reported row count. The simple protocol reports
    /// zero both for commands that affected no rows and commands without a row
    /// count, so callers must not infer the command kind from this value.
    pub rows_affected: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError {
    pub message: String,
    /// Byte offset into the submitted statement, when the server reports one.
    /// Used to point at the offending token instead of the whole statement.
    pub position: Option<usize>,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DbError {}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
///
/// ponytail: one mutex per connection, so queries on a profile serialise. A
/// profile runs one query at a time by design; revisit only if concurrent
/// statements per connection become a feature.
#[derive(Clone)]
pub struct Connection {
    client: Arc<Mutex<Client>>,
}

impl Connection {
    pub fn open(config: ConnectionConfig) -> Result<Self, DbError> {
        let client = Client::connect(&config.connection_string(), NoTls)
            .map_err(|error| connect_error(&error, &config))?;

        Ok(Self {
            client: Arc::new(Mutex::new(client)),
        })
    }

    /// Run one statement verbatim.
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. Row
    /// limits belong to the caller that *generated* a query, never to one the
    /// user typed.
    pub fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        let started = Instant::now();

        let mut client = self.client.lock().map_err(|_| DbError {
            message: "The connection is unavailable after an earlier internal failure.".into(),
            position: None,
        })?;

        let messages = client
            .simple_query(sql)
            .map_err(|error| query_error(&error, sql))?;

        Ok(assemble(messages, started.elapsed()))
    }
}

fn assemble(messages: Vec<SimpleQueryMessage>, elapsed: Duration) -> QueryResult {
    let mut result = QueryResult {
        elapsed,
        ..Default::default()
    };

    for message in messages {
        match message {
            SimpleQueryMessage::Row(row) => {
                if result.columns.is_empty() {
                    result.columns = row
                        .columns()
                        .iter()
                        .map(|column| Column {
                            name: column.name().to_string(),
                        })
                        .collect();
                }

                let cells: Vec<Cell> = (0..row.len())
                    .map(|index| row.get(index).map(str::to_string))
                    .collect();

                result.bytes += cells
                    .iter()
                    .filter_map(|cell| cell.as_ref().map(String::len))
                    .sum::<usize>();
                result.rows.push(cells);
            }
            SimpleQueryMessage::CommandComplete(count) => {
                result.rows_affected = Some(count);
            }
            _ => {}
        }
    }

    result
}

fn connect_error(error: &postgres::Error, config: &ConnectionConfig) -> DbError {
    // A refused connection is the most common failure by a wide margin, and the
    // driver's own wording buries the endpoint. Say what happened, and nothing
    // about what the user should do -- we cannot see their machine.
    if let Some(io) = io_source(error)
        && io.kind() == std::io::ErrorKind::ConnectionRefused
    {
        return DbError {
            message: format!(
                "Connection refused: nothing is listening on {}",
                config.endpoint()
            ),
            position: None,
        };
    }

    DbError {
        message: describe(error),
        position: None,
    }
}

fn query_error(error: &postgres::Error, sql: &str) -> DbError {
    let position = error.as_db_error().and_then(|db| match db.position() {
        // Postgres reports a 1-based character position into the statement.
        Some(postgres::error::ErrorPosition::Original(p)) => {
            character_position_to_byte_offset(sql, *p)
        }
        _ => None,
    });

    DbError {
        message: describe(error),
        position,
    }
}

fn character_position_to_byte_offset(sql: &str, position: u32) -> Option<usize> {
    let character_index = position.checked_sub(1)? as usize;
    sql.char_indices()
        .map(|(byte_offset, _)| byte_offset)
        .chain(std::iter::once(sql.len()))
        .nth(character_index)
}

/// Prefer the server's own message. It is written for humans and already says
/// the useful part; the driver's wrapper text mostly repeats "db error".
fn describe(error: &postgres::Error) -> String {
    if let Some(db) = error.as_db_error() {
        let mut message = db.message().to_string();
        if let Some(detail) = db.detail() {
            message.push('\n');
            message.push_str(detail);
        }
        if let Some(hint) = db.hint() {
            message.push('\n');
            message.push_str(hint);
        }
        return message;
    }

    error.to_string()
}

fn io_source(error: &postgres::Error) -> Option<&std::io::Error> {
    let mut source = std::error::Error::source(error);
    while let Some(current) = source {
        if let Some(io) = current.downcast_ref::<std::io::Error>() {
            return Some(io);
        }
        source = current.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ConnectionConfig {
        ConnectionConfig {
            host: "db.example.test".into(),
            port: 8432,
            database: "slate_test".into(),
            user: "someone".into(),
            password: String::new(),
        }
    }

    #[test]
    fn connection_string_quotes_values() {
        let config = config();
        assert_eq!(
            config.connection_string(),
            "host='db.example.test' port=8432 dbname='slate_test' user='someone'"
        );
    }

    #[test]
    fn blank_password_is_omitted_not_sent_empty() {
        // An empty `password=''` is not the same as offering no password, and
        // IAM auth relies on the latter.
        let config = config();
        assert!(!config.connection_string().contains("password"));
    }

    #[test]
    fn username_with_at_sign_survives() {
        // Cloud IAM usernames are email addresses. A bare interpolation would
        // still work here, but quoting is what keeps it working.
        let config = ConnectionConfig {
            user: "person@example.com".into(),
            ..config()
        };
        assert!(
            config
                .connection_string()
                .contains("user='person@example.com'")
        );
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        let config = ConnectionConfig {
            password: r"pa'ss\word".into(),
            ..config()
        };
        assert!(
            config
                .connection_string()
                .contains(r"password='pa\'ss\\word'")
        );
    }

    #[test]
    fn spaces_in_values_do_not_split_the_string() {
        let config = ConnectionConfig {
            database: "my database".into(),
            ..config()
        };
        assert!(config.connection_string().contains("dbname='my database'"));
    }

    #[test]
    fn server_character_positions_become_utf8_byte_offsets() {
        let sql = "SELECT 'é', broken";
        let broken_byte_offset = sql.find("broken").unwrap();
        let broken_character_position = sql[..broken_byte_offset].chars().count() as u32 + 1;
        assert_eq!(
            character_position_to_byte_offset(sql, broken_character_position),
            Some(broken_byte_offset)
        );
    }

    #[test]
    fn invalid_server_character_positions_are_rejected() {
        assert_eq!(character_position_to_byte_offset("SELECT 1", 0), None);
        assert_eq!(character_position_to_byte_offset("SELECT 1", 100), None);
    }

    #[test]
    #[ignore = "requires a local Postgres server configured through PG*"]
    fn live_query_round_trip() {
        let config = ConnectionConfig {
            host: std::env::var("PGHOST").expect("PGHOST is required"),
            port: std::env::var("PGPORT")
                .expect("PGPORT is required")
                .parse()
                .expect("PGPORT must be a number"),
            database: std::env::var("PGDATABASE").expect("PGDATABASE is required"),
            user: std::env::var("PGUSER").expect("PGUSER is required"),
            password: std::env::var("PGPASSWORD").unwrap_or_default(),
        };

        let connection = Connection::open(config).expect("connection should open");
        let result = connection
            .query("SELECT * FROM (VALUES (1, 'alpha'), (2, NULL)) AS sample(id, label)")
            .expect("query should succeed");

        assert_eq!(
            result.columns,
            vec![
                Column { name: "id".into() },
                Column {
                    name: "label".into()
                }
            ]
        );
        assert_eq!(
            result.rows,
            vec![
                vec![Some("1".into()), Some("alpha".into())],
                vec![Some("2".into()), None]
            ]
        );
        assert_eq!(result.bytes, 7);
        assert_eq!(result.rows_affected, Some(2));
    }
}
