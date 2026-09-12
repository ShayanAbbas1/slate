//! The database boundary.
//!
//! Everything crossing out of this module is a rendered `String`. No
//! `postgres::Row`, no `rusqlite::ValueRef`, no OIDs — the UI layer never
//! learns which engine it is talking to. Dispatch is the [`Connection`] enum
//! below, and it stops here (AGENTS.md, hard rule 4).
//!
//! Each engine module owns everything about its own driver: how a value becomes
//! text, what a type is called, which catalog answers a question. What they
//! share is the vocabulary in this file — and the assemblers that turn a
//! [`QueryResult`] into a [`Catalog`], a [`Structure`] or its
//! [`ForeignKey`]s, which is why an
//! engine's catalog SQL aliases its columns to names chosen here rather than to
//! its own.

use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use crate::tls::SslMode;

mod mysql;
mod postgres;
mod sqlite;

/// Which engine a profile talks to.
///
/// Also the answer to the only three questions Slate's own generated SQL asks
/// about dialect. There being three is why there is no `Dialect` type: an
/// engine quotes an identifier, quotes a literal, and qualifies a name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Engine {
    #[default]
    Postgres,
    MySql,
    Sqlite,
}

impl Engine {
    /// Presentation order, which is the order the form's chips appear in.
    pub const ALL: [Self; 3] = [Self::Postgres, Self::MySql, Self::Sqlite];

    pub fn label(self) -> &'static str {
        match self {
            Self::Postgres => "Postgres",
            Self::MySql => "MySQL",
            Self::Sqlite => "SQLite",
        }
    }

    /// The spelling stored in `profiles.toml`. Changing one of these strings
    /// orphans every profile already written with it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::MySql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }

    /// Accepts the URL schemes as well as the stored spellings, so one function
    /// serves both the profile reader and the URL box.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" => Ok(Self::Postgres),
            "mysql" | "mariadb" => Ok(Self::MySql),
            "sqlite" | "sqlite3" | "file" => Ok(Self::Sqlite),
            other => Err(format!("{other} is not a database engine Slate speaks.")),
        }
    }

    /// Whether the engine reaches a server rather than opening a file.
    /// Everything a server needs — a host, credentials, TLS — is absent for the
    /// one that does not, and this is what the form asks before drawing them.
    pub fn is_server(self) -> bool {
        !matches!(self, Self::Sqlite)
    }

    /// The word that opens a transaction around a generated multi-statement
    /// batch, or `None` for the engine that already makes one submission
    /// atomic. `COMMIT` and `ROLLBACK` are spelled the same everywhere, so the
    /// opening word is the whole of the difference.
    ///
    /// MySQL's own spelling is `START TRANSACTION`, and `BEGIN` is its
    /// documented alias outside a stored program. The alias is what Slate
    /// writes because the brackets go into the statement text, where
    /// `sql::is_generated_write` has to read them back: the tree-sitter
    /// grammar has no `START TRANSACTION`, so the gate would refuse Slate's own
    /// batch.
    pub fn transaction_start(self) -> Option<&'static str> {
        match self {
            // One simple-query submission is already one implicit transaction.
            Self::Postgres => None,
            Self::MySql => Some("BEGIN"),
            Self::Sqlite => Some("BEGIN"),
        }
    }

    /// Postgres and SQLite take the standard's double quote. MySQL takes a
    /// backtick, which it accepts whether or not `ANSI_QUOTES` is set — a double
    /// quote there is a *string literal*, so quoting a MySQL identifier the
    /// standard way produces a statement that runs and means something else.
    fn identifier_quote(self) -> char {
        match self {
            Self::Postgres | Self::Sqlite => '"',
            Self::MySql => '`',
        }
    }

    pub fn quote_identifier(self, identifier: &str) -> String {
        let quote = self.identifier_quote();
        format!(
            "{quote}{}{quote}",
            identifier.replace(quote, &format!("{quote}{quote}"))
        )
    }

    /// The inverse, for reading back a name Slate wrote — matching a sort key in
    /// a statement to the column header it belongs to, say.
    ///
    /// Anything that is not a quoted identifier comes back unchanged: a bare
    /// position or a function call names no column, and pretending otherwise
    /// would light up the wrong header.
    pub fn unquote_identifier(self, expression: &str) -> String {
        let quote = self.identifier_quote();
        match expression
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            Some(inner) => inner.replace(&format!("{quote}{quote}"), &quote.to_string()),
            None => expression.to_string(),
        }
    }

    /// Doubling the quote is enough for two of the three: neither Postgres nor
    /// SQLite reads a backslash as an escape, the first because
    /// `standard_conforming_strings` is on by default and the second because it
    /// has no such notion at all. MySQL does, unless `NO_BACKSLASH_ESCAPES` is
    /// set — which is again not Slate's to set — so a literal backslash has to
    /// survive as two.
    pub fn quote_literal(self, value: &str) -> String {
        match self {
            Self::Postgres | Self::Sqlite => format!("'{}'", value.replace('\'', "''")),
            Self::MySql => format!("'{}'", value.replace('\\', r"\\").replace('\'', "''")),
        }
    }

    pub fn qualified(self, schema: &str, name: &str) -> String {
        format!(
            "{}.{}",
            self.quote_identifier(schema),
            self.quote_identifier(name)
        )
    }
}

/// A URL is percent-encoded by definition, and a path with a space in it is
/// ordinary on macOS.
pub(super) fn percent_decoded(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let digits = value
                .get(index + 1..index + 3)
                .ok_or_else(|| "Connection URL ends in an incomplete escape.".to_string())?;
            decoded
                .push(u8::from_str_radix(digits, 16).map_err(|_| {
                    format!("Connection URL contains an invalid escape %{digits}.")
                })?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }

    String::from_utf8(decoded).map_err(|_| "Connection URL path is not valid UTF-8.".to_string())
}

/// What an engine needs to reach a server. SQLite has none of it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerConfig {
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    /// Blank is valid and must never be warned about — cloud IAM auth issues a
    /// short-lived token as the password, or none at all.
    pub password: String,
    pub sslmode: SslMode,
    /// libpq's `sslrootcert`. Replaces the platform's trust store rather than
    /// adding to it, and only consulted by the two verifying modes.
    pub root_certificate: Option<String>,
    /// How long a statement may run, in seconds, or 0 for no limit.
    ///
    /// A number here and nothing else: how it is expressed is a question each
    /// engine module answers for itself (AGENTS.md, hard rule 4). What the
    /// answers cost is worth knowing, because they are not the same bargain:
    ///
    /// - Postgres sets `statement_timeout`, which bounds any statement.
    /// - MySQL sets `max_execution_time`, which bounds **read-only `SELECT`s
    ///   only** -- a runaway `UPDATE` or `ALTER` runs to completion and Cancel
    ///   is the only recourse against it. The server has also only had the
    ///   variable since 5.7.8, and MariaDB spells it differently, so asking for
    ///   a timeout there fails the connect rather than the statement.
    /// - SQLite has no such setting and gets a wall-clock timer firing
    ///   [`Connection::cancel`]'s interrupt instead, so it counts time a
    ///   statement spent blocked on a lock as readily as time it spent scanning.
    ///
    /// Applied once at connect, as a session default, never spliced into the
    /// user's submission -- rewriting what they typed is hard rule 1, and on
    /// Postgres a `SET` inside their submission would be scoped to the implicit
    /// transaction around it and change what their own `BEGIN` means. The other
    /// side of that: a user who runs their own `SET statement_timeout = 0`
    /// silently wins for the rest of the session, which is correct.
    pub statement_timeout: u32,
}

impl ServerConfig {
    pub fn endpoint(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }
}

/// Where a profile connects.
///
/// An enum rather than one struct carrying an engine tag: SQLite has no host,
/// no port, no user, no password and no TLS. Six permanently-empty fields would
/// be six dead inputs on the form, six dead keys in `profiles.toml`, and a
/// blank host that every layer below has to keep deciding is fine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionConfig {
    Postgres(ServerConfig),
    MySql(ServerConfig),
    Sqlite {
        path: String,
        statement_timeout: u32,
    },
}

impl ConnectionConfig {
    pub fn engine(&self) -> Engine {
        match self {
            Self::Postgres(_) => Engine::Postgres,
            Self::MySql(_) => Engine::MySql,
            Self::Sqlite { .. } => Engine::Sqlite,
        }
    }

    /// The server half, for the callers that only have something to say when
    /// there is one — the credential fields, and the Keychain.
    pub fn server(&self) -> Option<&ServerConfig> {
        match self {
            Self::Postgres(server) | Self::MySql(server) => Some(server),
            Self::Sqlite { .. } => None,
        }
    }

    /// The same, for the one caller that fills the password in: connecting
    /// reads it from the Keychain, which the profile on disk never holds.
    pub fn server_mut(&mut self) -> Option<&mut ServerConfig> {
        match self {
            Self::Postgres(server) | Self::MySql(server) => Some(server),
            Self::Sqlite { .. } => None,
        }
    }

    /// The scheme picks the engine, and the engine parses the rest. Slate never
    /// guesses from the shape of a URL: a host-looking string is a host to
    /// three different drivers.
    pub fn from_url(url: &str) -> Result<Self, String> {
        let scheme = url
            .split_once("://")
            .or_else(|| url.split_once(':'))
            .map(|(scheme, _)| scheme)
            .filter(|scheme| !scheme.is_empty())
            .ok_or_else(|| {
                "Connection URL must start with a scheme, such as postgresql:// or sqlite://."
                    .to_string()
            })?;

        match Engine::parse(scheme).map_err(|_| {
            format!("Connection URL scheme {scheme}:// is not a database Slate speaks.")
        })? {
            Engine::Postgres => postgres::config_from_url(url).map(Self::Postgres),
            Engine::MySql => mysql::config_from_url(url).map(Self::MySql),
            Engine::Sqlite => sqlite::path_from_url(url).map(|path| Self::Sqlite {
                path,
                // A URL has nowhere to say it; the form is where it is set.
                statement_timeout: 0,
            }),
        }
    }

    /// The statement timeout in seconds, or 0 for none. One accessor rather
    /// than a match at every call site, since no caller above `src/db/` cares
    /// which variant is carrying it.
    pub fn statement_timeout(&self) -> u32 {
        match self {
            Self::Postgres(server) | Self::MySql(server) => server.statement_timeout,
            Self::Sqlite {
                statement_timeout, ..
            } => *statement_timeout,
        }
    }

    /// Whether replacing `self` with `edited` has to be reconnected for.
    ///
    /// A blank password in `edited` is not a change: the edit form never shows
    /// what the Keychain holds, so blank there means "leave it alone" -- taken
    /// literally it would drop and reopen the connection every time a colour
    /// was saved. A password that was typed does count, since applying it is
    /// the only reason to type one.
    pub fn needs_reconnect(&self, edited: &Self) -> bool {
        let mut current = self.clone();
        if let Some(server) = current.server_mut()
            && edited
                .server()
                .is_some_and(|server| server.password.is_empty())
        {
            server.password.clear();
        }
        current != *edited
    }

    /// What was being talked to, for an error or a title to name.
    pub fn endpoint(&self) -> String {
        match self {
            Self::Postgres(server) | Self::MySql(server) => server.endpoint(),
            Self::Sqlite { path, .. } => path.clone(),
        }
    }
}

/// A live connection. Cloneable so a background task can take one without
/// borrowing the view.
#[derive(Clone)]
pub enum Connection {
    Postgres(postgres::Connection),
    MySql(mysql::Connection),
    Sqlite(sqlite::Connection),
}

impl Connection {
    pub fn open(config: ConnectionConfig) -> Result<Self, DbError> {
        match config {
            ConnectionConfig::Postgres(server) => {
                postgres::Connection::open(&server).map(Self::Postgres)
            }
            ConnectionConfig::MySql(server) => mysql::Connection::open(&server).map(Self::MySql),
            ConnectionConfig::Sqlite {
                path,
                statement_timeout,
            } => sqlite::Connection::open(&path, statement_timeout).map(Self::Sqlite),
        }
    }

    /// Run one statement verbatim.
    ///
    /// The SQL is never rewritten — no limit injected, no reformatting. Row
    /// limits belong to the caller that *generated* a query, never to one the
    /// user typed.
    pub fn query(&self, sql: &str) -> Result<QueryResult, DbError> {
        match self {
            Self::Postgres(connection) => connection.query(sql),
            Self::MySql(connection) => connection.query(sql),
            Self::Sqlite(connection) => connection.query(sql),
        }
    }

    pub fn catalog(&self) -> Result<Catalog, DbError> {
        match self {
            Self::Postgres(connection) => connection.catalog(),
            Self::MySql(connection) => connection.catalog(),
            Self::Sqlite(connection) => connection.catalog(),
        }
    }

    pub fn structure(&self, schema: &str, relation: &str) -> Result<Structure, DbError> {
        match self {
            Self::Postgres(connection) => connection.structure(schema, relation),
            Self::MySql(connection) => connection.structure(schema, relation),
            Self::Sqlite(connection) => connection.structure(schema, relation),
        }
    }

    /// Ask the server to stop whatever this connection is running.
    ///
    /// Takes `&self` and touches the connection mutex nowhere, deliberately:
    /// the runaway statement is holding that mutex, so a cancel that waited for
    /// it would deadlock against the very query it exists to stop. Every handle
    /// this needs is therefore taken in each engine's `open`, off the client,
    /// before the client is moved in behind the mutex.
    ///
    /// There is no cancelled state anywhere above this: a stopped statement
    /// comes back out of [`Connection::query`] as an ordinary `Err` carrying the
    /// server's own words, which is a truer account than Slate could write.
    ///
    /// What it cannot do. It reaches only the statement running on *this*
    /// connection, so a catalog or structure load queued behind it on the same
    /// mutex is untouched -- the profile is still frozen until the running
    /// statement lets go. And it is the *slow* runaway it helps with, not the
    /// fat one: Postgres buffers a whole result set before Slate sees a row, so
    /// a query already returning gigabytes is past the point where stopping the
    /// server helps.
    pub fn cancel(&self) -> Result<(), DbError> {
        match self {
            Self::Postgres(connection) => connection.cancel(),
            Self::MySql(connection) => connection.cancel(),
            Self::Sqlite(connection) => connection.cancel(),
        }
    }
}

/// One column of a result set.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// The server's own name for the column's type — `int4`, `jsonb`,
    /// `timestamptz` — as a Slate-owned string, never a driver type.
    ///
    /// Absent rather than guessed. The simple query protocol carries no type
    /// information at all, so this is learned by describing the statement, and
    /// Postgres will not describe everything (see [`column_types`]).
    pub data_type: Option<String>,
}

/// Whether a [`Column::data_type`] names a type whose values are bytes.
///
/// Three engines' spellings in one predicate rather than three, because nothing
/// above this module is allowed to know which engine answered (AGENTS.md, hard
/// rule 4). Substrings because the families are open-ended in two directions:
/// MySQL prefixes its blobs and binaries, and SQLite gives BLOB affinity to any
/// declared type merely *containing* `blob`.
///
/// What it is for: a blob is rendered as the engine's own literal — `x'AB'`,
/// `0xAB` — and [`Engine::quote_literal`] would quote that back as the six
/// characters it looks like, so an edited blob column becomes text. The grid
/// refuses the edit instead. Absent here means unknown, not text: a driver that
/// could not name the type says nothing, and treating silence as binary would
/// make ordinary columns read-only.
pub fn is_binary_type(data_type: &str) -> bool {
    let name = data_type.to_ascii_lowercase();
    name == "bytea" || name.contains("blob") || name.contains("binary")
}

/// A cell value, already formatted by the server. `None` is SQL NULL, which is
/// distinct from an empty string and must stay distinguishable in the grid.
pub type Cell = Option<String>;

/// Serialized because a restored tab has to know which kind it is before the
/// catalog that would say so has loaded -- and a table is what a profile
/// written before the kind was stored gets read back as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    #[default]
    Table,
    PartitionedTable,
    View,
    MaterializedView,
    ForeignTable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub name: String,
    pub kind: RelationKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoutineKind {
    Function,
    Procedure,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Routine {
    pub name: String,
    pub kind: RoutineKind,
    pub identity_arguments: String,
    pub result_type: String,
    pub language: String,
    pub definition: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schema {
    pub name: String,
    pub relations: Vec<Relation>,
    pub routines: Vec<Routine>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Catalog {
    pub schemas: Vec<Schema>,
}

/// One relation's definition. Loaded when the relation is opened rather than at
/// connect: a database with thousands of relations would pay for every one of
/// them to show the columns of the one that was clicked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Structure {
    pub columns: Vec<ColumnDefinition>,
    pub indexes: Vec<NamedDefinition>,
    pub constraints: Vec<NamedDefinition>,
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
}

/// An index or a constraint, as the name plus the server's own rendering of it.
/// Postgres already prints both as readable DDL, so parsing them into fields
/// would only lose information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedDefinition {
    pub name: String,
    pub definition: String,
}

/// One column of one relation, and the column it references. Enough to write a
/// `WHERE` against the referenced relation and nothing else.
///
/// A composite key is several of these and nothing groups them: following a key
/// is a per-column gesture, so the constraint they came from is not something a
/// caller has to reassemble.
///
/// Additional to the rendered DDL in [`Structure::constraints`], not a
/// replacement for it — the text is what the structure tab shows, and parsing a
/// server's rendering back into fields would only lose information. Computing
/// the fields from the catalog that text was rendered from loses nothing.
///
/// Engine-agnostic by rule, not by accident: hard rule 4. No oid, no attribute
/// number, no `information_schema` row and no driver value reaches these four
/// owned strings, and nothing here says which engine answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForeignKey {
    pub column: String,
    pub referenced_schema: String,
    pub referenced_table: String,
    pub referenced_column: String,
}

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
    /// Where these rows can be written back to, when they can be at all.
    /// `None` is the answer for every result set Slate cannot address a single
    /// row of, and it is not an error — see [`Connection::edit_target`].
    pub edit: Option<EditTarget>,
}

/// The table a result set's rows can be written back to, already resolved to
/// names and result-column positions.
///
/// The identity work — which oid, which attribute number — happens inside this
/// module and stops here (hard rule 4). A caller gets an answer it can build
/// SQL from, not a puzzle it has to ask the catalog about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditTarget {
    pub schema: String,
    pub table: String,
    /// The real column name behind each result column, positionally. `None`
    /// where the result column is computed rather than read from the table, so
    /// `SELECT id AS ident, count(*)` gives `[Some("id"), None]`.
    pub columns: Vec<Option<String>>,
    /// Result-column indices that together identify one row. Never empty.
    pub keys: Vec<usize>,
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

pub(super) fn assemble_catalog(
    relations: QueryResult,
    routines: QueryResult,
) -> Result<Catalog, DbError> {
    let mut schemas = std::collections::BTreeMap::<String, Schema>::new();

    for row in &relations.rows {
        let schema_name = required_cell(&relations, row, "schema_name")?;
        let name = required_cell(&relations, row, "relation_name")?;
        let kind = match required_cell(&relations, row, "relation_kind")? {
            "table" => RelationKind::Table,
            "partitioned_table" => RelationKind::PartitionedTable,
            "view" => RelationKind::View,
            "materialized_view" => RelationKind::MaterializedView,
            "foreign_table" => RelationKind::ForeignTable,
            kind => return Err(unexpected_catalog_value("relation kind", kind)),
        };

        schema(&mut schemas, schema_name).relations.push(Relation {
            name: name.to_string(),
            kind,
        });
    }

    for row in &routines.rows {
        let schema_name = required_cell(&routines, row, "schema_name")?;
        let name = required_cell(&routines, row, "routine_name")?;
        let kind = match required_cell(&routines, row, "routine_kind")? {
            "function" => RoutineKind::Function,
            "procedure" => RoutineKind::Procedure,
            kind => return Err(unexpected_catalog_value("routine kind", kind)),
        };

        schema(&mut schemas, schema_name).routines.push(Routine {
            name: name.to_string(),
            kind,
            identity_arguments: required_cell(&routines, row, "identity_arguments")?.to_string(),
            result_type: required_cell(&routines, row, "result_type")?.to_string(),
            language: required_cell(&routines, row, "language")?.to_string(),
            definition: required_cell(&routines, row, "definition")?.to_string(),
        });
    }

    Ok(Catalog {
        schemas: schemas.into_values().collect(),
    })
}

pub(super) fn assemble_structure(
    columns: QueryResult,
    indexes: QueryResult,
    constraints: QueryResult,
) -> Result<Structure, DbError> {
    let mut structure = Structure::default();

    for row in &columns.rows {
        let default = required_cell(&columns, row, "column_default")?;
        structure.columns.push(ColumnDefinition {
            name: required_cell(&columns, row, "column_name")?.to_string(),
            data_type: required_cell(&columns, row, "data_type")?.to_string(),
            nullable: match required_cell(&columns, row, "nullable")? {
                "yes" => true,
                "no" => false,
                value => return Err(unexpected_catalog_value("nullability", value)),
            },
            default: (!default.is_empty()).then(|| default.to_string()),
        });
    }

    for (result, into) in [
        (&indexes, &mut structure.indexes),
        (&constraints, &mut structure.constraints),
    ] {
        for row in &result.rows {
            into.push(NamedDefinition {
                name: required_cell(result, row, "object_name")?.to_string(),
                definition: required_cell(result, row, "definition")?.to_string(),
            });
        }
    }

    Ok(structure)
}

/// Reads foreign keys out of a result whose columns are named the way Slate
/// names them: `column_name`, `referenced_schema`, `referenced_table`,
/// `referenced_column`.
///
/// This is shared *parsing of a Slate-named result shape*, not shared dispatch.
/// Postgres and MySQL each write their own catalog query and each choose these
/// four aliases, so the row-to-struct step is the same work twice; SQLite does
/// not use this at all, because `PRAGMA foreign_key_list` reports a different
/// shape. No engine branches here and no engine has to route through it.
pub(super) fn assemble_foreign_keys(result: &QueryResult) -> Result<Vec<ForeignKey>, DbError> {
    result
        .rows
        .iter()
        .map(|row| {
            Ok(ForeignKey {
                column: required_cell(result, row, "column_name")?.to_string(),
                referenced_schema: required_cell(result, row, "referenced_schema")?.to_string(),
                referenced_table: required_cell(result, row, "referenced_table")?.to_string(),
                referenced_column: required_cell(result, row, "referenced_column")?.to_string(),
            })
        })
        .collect()
}

fn schema<'a>(
    schemas: &'a mut std::collections::BTreeMap<String, Schema>,
    name: &str,
) -> &'a mut Schema {
    schemas.entry(name.to_string()).or_insert_with(|| Schema {
        name: name.to_string(),
        relations: Vec::new(),
        routines: Vec::new(),
    })
}

pub(super) fn required_cell<'a>(
    result: &'a QueryResult,
    row: &'a [Cell],
    column_name: &str,
) -> Result<&'a str, DbError> {
    let index = result
        .columns
        .iter()
        .position(|column| column.name == column_name)
        .ok_or_else(|| plain_error(format!("Catalog query omitted column {column_name}.")))?;

    row.get(index)
        .and_then(Option::as_deref)
        .ok_or_else(|| plain_error(format!("Catalog query returned no {column_name}.")))
}

pub(super) fn unexpected_catalog_value(label: &str, value: &str) -> DbError {
    plain_error(format!("Catalog query returned unknown {label} {value}."))
}

pub(super) fn non_utf8_error(columns: &[Column], index: usize) -> DbError {
    let column = columns
        .get(index)
        .map(|column| format!("column {}", column.name))
        .unwrap_or_else(|| format!("column {index}"));

    plain_error(format!(
        "A value in {column} is not valid UTF-8 text and cannot be displayed."
    ))
}

pub(super) fn plain_error(message: String) -> DbError {
    DbError {
        message,
        position: None,
    }
}

/// Shared with `postgres::tests`, which exercises `assemble`'s type-matching
/// against the same synthetic result shape.
#[cfg(test)]
pub(super) fn result(columns: &[&str], rows: &[&[Option<&str>]]) -> QueryResult {
    QueryResult {
        columns: columns
            .iter()
            .map(|name| Column {
                name: (*name).to_string(),
                ..Default::default()
            })
            .collect(),
        rows: rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.map(str::to_string)).collect())
            .collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_engine_round_trips_through_the_spelling_it_is_stored_as() {
        // `as_str` is what lands in `profiles.toml`. If `parse` ever stopped
        // accepting one of them, every profile written with it would fail to
        // load with no way back.
        for engine in Engine::ALL {
            assert_eq!(Engine::parse(engine.as_str()), Ok(engine));
        }
    }

    #[test]
    fn a_url_scheme_picks_the_engine_and_an_unknown_one_is_named() {
        assert_eq!(
            ConnectionConfig::from_url("postgresql://someone@db.example.test/slate_test")
                .unwrap()
                .engine(),
            Engine::Postgres
        );
        assert_eq!(
            ConnectionConfig::from_url("sqlite:///tmp/slate.db").unwrap(),
            ConnectionConfig::Sqlite {
                path: "/tmp/slate.db".into(),
                statement_timeout: 0
            }
        );

        // Named, not merely rejected: "invalid URL" leaves the user guessing
        // which part of it Slate objected to.
        let error = ConnectionConfig::from_url("mongodb://db.example.test/slate").unwrap_err();
        assert!(error.contains("mongodb"), "{error}");
        assert!(ConnectionConfig::from_url("db.example.test/slate").is_err());
    }

    #[test]
    fn a_sqlite_profile_has_no_server_half_and_a_postgres_one_does() {
        assert!(
            ConnectionConfig::Sqlite {
                path: "/tmp/slate.db".into(),
                statement_timeout: 0
            }
            .server()
            .is_none()
        );
        assert!(
            ConnectionConfig::Postgres(ServerConfig::default())
                .server()
                .is_some()
        );
    }

    #[test]
    fn an_endpoint_names_whatever_was_being_talked_to() {
        assert_eq!(
            ConnectionConfig::Sqlite {
                path: "/tmp/slate.db".into(),
                statement_timeout: 0
            }
            .endpoint(),
            "/tmp/slate.db"
        );
        assert_eq!(
            ServerConfig {
                host: "db.example.test".into(),
                port: Some(8432),
                ..ServerConfig::default()
            }
            .endpoint(),
            "db.example.test:8432"
        );
        assert_eq!(
            ServerConfig {
                host: "db.example.test".into(),
                port: None,
                ..ServerConfig::default()
            }
            .endpoint(),
            "db.example.test"
        );
    }

    #[test]
    fn an_edit_reconnects_for_a_new_destination_but_not_for_a_blank_password() {
        let stored = ConnectionConfig::Postgres(ServerConfig {
            host: "db.example.test".into(),
            port: Some(5432),
            database: "slate".into(),
            user: "someone".into(),
            password: "secret".into(),
            ..ServerConfig::default()
        });
        let edited = |change: fn(&mut ServerConfig)| {
            let mut server = stored.server().unwrap().clone();
            // What the form hands back: it never fills the password in.
            server.password.clear();
            change(&mut server);
            ConnectionConfig::Postgres(server)
        };

        assert!(!stored.needs_reconnect(&stored.clone()));
        assert!(!stored.needs_reconnect(&edited(|_| {})));
        assert!(stored.needs_reconnect(&edited(|server| server.host = "elsewhere".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.port = Some(6432))));
        assert!(stored.needs_reconnect(&edited(|server| server.database = "other".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.user = "someone_else".into())));
        assert!(stored.needs_reconnect(&edited(|server| server.password = "typed".into())));
        assert!(stored.needs_reconnect(&ConnectionConfig::MySql(stored.server().unwrap().clone())));
        assert!(stored.needs_reconnect(&ConnectionConfig::Sqlite {
            path: "/tmp/slate.db".into(),
            statement_timeout: 0
        }));
    }

    #[test]
    fn only_the_engine_with_an_implicit_transaction_needs_no_brackets() {
        // Postgres runs one submission as one transaction; the other two commit
        // each statement on its own and have to be told. `BEGIN` rather than
        // MySQL's own `START TRANSACTION` because the brackets are read back by
        // `sql::is_generated_write`, whose grammar knows only the first.
        assert_eq!(Engine::Postgres.transaction_start(), None);
        assert_eq!(Engine::MySql.transaction_start(), Some("BEGIN"));
        assert_eq!(Engine::Sqlite.transaction_start(), Some("BEGIN"));
    }

    #[test]
    fn each_engine_quotes_the_way_its_own_server_reads() {
        // An identifier and a literal are both user data, and both reach a
        // statement Slate generates. The escape is what stops a table called
        // `odd"name` from ending the identifier early.
        assert_eq!(
            Engine::Postgres.quote_identifier("odd\"name"),
            "\"odd\"\"name\""
        );
        assert_eq!(
            Engine::Sqlite.quote_identifier("odd\"name"),
            "\"odd\"\"name\""
        );
        assert_eq!(Engine::MySql.quote_identifier("odd`name"), "`odd``name`");

        for engine in Engine::ALL {
            assert_eq!(
                engine.quote_literal("odd'value"),
                "'odd''value'",
                "{engine:?}"
            );
        }

        // Only MySQL reads a backslash as an escape, so only MySQL has to
        // double one. Getting this wrong is how a trailing backslash turns the
        // closing quote into an escaped one and swallows the rest of the
        // statement.
        assert_eq!(Engine::MySql.quote_literal(r"back\slash"), r"'back\\slash'");
        assert_eq!(
            Engine::Postgres.quote_literal(r"back\slash"),
            r"'back\slash'"
        );
        assert_eq!(Engine::Sqlite.quote_literal(r"back\slash"), r"'back\slash'");

        assert_eq!(
            Engine::Postgres.qualified("odd\"schema", "table"),
            "\"odd\"\"schema\".\"table\""
        );
        assert_eq!(
            Engine::MySql.qualified("slate_dev", "table"),
            "`slate_dev`.`table`"
        );
    }

    #[test]
    fn a_quoted_identifier_reads_back_as_the_name_it_was() {
        // The two halves have to agree or Slate cannot recognise its own
        // output: a sort key it wrote would not match the header it came from.
        for engine in Engine::ALL {
            for name in ["id", "odd\"name", "odd`name", "spaced name", ""] {
                assert_eq!(
                    engine.unquote_identifier(&engine.quote_identifier(name)),
                    name,
                    "{engine:?} {name}"
                );
            }

            // Not a quoted identifier, so not a name. A bare position and a
            // function call both have to survive untouched.
            assert_eq!(engine.unquote_identifier("3"), "3");
            assert_eq!(engine.unquote_identifier("lower(name)"), "lower(name)");
        }
    }

    #[test]
    fn catalog_groups_relations_and_routines_by_schema() {
        let relations = result(
            &["schema_name", "relation_name", "relation_kind"],
            &[
                &[Some("analytics"), Some("events"), Some("partitioned_table")],
                &[Some("public"), Some("accounts"), Some("table")],
                &[Some("public"), Some("account_overview"), Some("view")],
            ],
        );
        let routines = result(
            &[
                "schema_name",
                "routine_name",
                "routine_kind",
                "identity_arguments",
                "result_type",
                "language",
                "definition",
            ],
            &[
                &[
                    Some("analytics"),
                    Some("refresh_events"),
                    Some("procedure"),
                    Some("full boolean"),
                    Some(""),
                    Some("plpgsql"),
                    Some("CREATE PROCEDURE analytics.refresh_events(full boolean)"),
                ],
                &[
                    Some("public"),
                    Some("account_name"),
                    Some("function"),
                    Some("account_id bigint"),
                    Some("text"),
                    Some("sql"),
                    Some("CREATE FUNCTION public.account_name(account_id bigint)"),
                ],
            ],
        );

        let catalog = assemble_catalog(relations, routines).unwrap();

        assert_eq!(catalog.schemas.len(), 2);
        assert_eq!(catalog.schemas[0].name, "analytics");
        assert_eq!(
            catalog.schemas[0].relations,
            vec![Relation {
                name: "events".into(),
                kind: RelationKind::PartitionedTable,
            }]
        );
        assert_eq!(catalog.schemas[0].routines[0].kind, RoutineKind::Procedure);
        assert_eq!(catalog.schemas[1].name, "public");
        assert_eq!(catalog.schemas[1].relations[1].kind, RelationKind::View);
        assert_eq!(catalog.schemas[1].routines[0].result_type, "text");
    }

    #[test]
    fn structure_reads_nullability_and_treats_a_blank_default_as_absent() {
        let columns = result(
            &["column_name", "data_type", "nullable", "column_default"],
            &[
                &[Some("id"), Some("bigint"), Some("no"), Some("nextval('s')")],
                &[Some("label"), Some("text"), Some("yes"), Some("")],
            ],
        );
        let indexes = result(
            &["object_name", "definition"],
            &[&[Some("accounts_pkey"), Some("CREATE UNIQUE INDEX …")]],
        );
        let constraints = result(
            &["object_name", "definition"],
            &[&[Some("accounts_pkey"), Some("PRIMARY KEY (id)")]],
        );

        let structure = assemble_structure(columns, indexes, constraints).unwrap();

        assert_eq!(
            structure.columns,
            vec![
                ColumnDefinition {
                    name: "id".into(),
                    data_type: "bigint".into(),
                    nullable: false,
                    default: Some("nextval('s')".into()),
                },
                ColumnDefinition {
                    name: "label".into(),
                    data_type: "text".into(),
                    nullable: true,
                    default: None,
                },
            ]
        );
        assert_eq!(structure.indexes[0].name, "accounts_pkey");
        assert_eq!(structure.constraints[0].definition, "PRIMARY KEY (id)");
    }

    #[test]
    fn a_composite_foreign_key_arrives_as_one_row_per_column() {
        let keys = result(
            &[
                "column_name",
                "referenced_schema",
                "referenced_table",
                "referenced_column",
            ],
            &[
                &[
                    Some("tenant_id"),
                    Some("public"),
                    Some("accounts"),
                    Some("tenant_id"),
                ],
                &[
                    Some("account_id"),
                    Some("public"),
                    Some("accounts"),
                    Some("id"),
                ],
            ],
        );

        assert_eq!(
            assemble_foreign_keys(&keys).unwrap(),
            vec![
                ForeignKey {
                    column: "tenant_id".into(),
                    referenced_schema: "public".into(),
                    referenced_table: "accounts".into(),
                    referenced_column: "tenant_id".into(),
                },
                ForeignKey {
                    column: "account_id".into(),
                    referenced_schema: "public".into(),
                    referenced_table: "accounts".into(),
                    referenced_column: "id".into(),
                },
            ]
        );
    }

    #[test]
    fn a_foreign_key_query_missing_a_column_is_an_error_not_a_guess() {
        let keys = result(
            &["column_name", "referenced_schema", "referenced_table"],
            &[&[Some("account_id"), Some("public"), Some("accounts")]],
        );

        let error = assemble_foreign_keys(&keys).unwrap_err();

        assert_eq!(
            error.message,
            "Catalog query omitted column referenced_column."
        );
    }

    #[test]
    fn the_rendered_constraints_survive_the_structured_form_arriving() {
        let columns = result(
            &["column_name", "data_type", "nullable", "column_default"],
            &[&[Some("id"), Some("bigint"), Some("no"), Some("")]],
        );
        let constraints = result(
            &["object_name", "definition"],
            &[&[
                Some("orders_account_id_fkey"),
                Some("FOREIGN KEY (account_id) REFERENCES public.accounts(id)"),
            ]],
        );

        let structure = assemble_structure(columns, QueryResult::default(), constraints).unwrap();

        assert_eq!(
            structure.constraints,
            vec![NamedDefinition {
                name: "orders_account_id_fkey".into(),
                definition: "FOREIGN KEY (account_id) REFERENCES public.accounts(id)".into(),
            }]
        );
        assert!(structure.foreign_keys.is_empty());
    }

    #[test]
    fn catalog_rejects_unknown_object_kinds() {
        let relations = result(
            &["schema_name", "relation_name", "relation_kind"],
            &[&[Some("public"), Some("mystery"), Some("unknown")]],
        );

        let error = assemble_catalog(relations, QueryResult::default()).unwrap_err();

        assert_eq!(
            error.message,
            "Catalog query returned unknown relation kind unknown."
        );
    }
}
