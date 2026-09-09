use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use security_framework::passwords::{self, PasswordOptions};
use serde::{Deserialize, Serialize};

const PROFILES_FILE: &str = "profiles.toml";
const KEYCHAIN_SERVICE: &str = "Slate";
const SCRATCH_FILE: &str = ".scratch.sql";
const HISTORY_FILE: &str = ".history.jsonl";
/// How far back the history reads.
///
/// ponytail: the whole file is read and the newest entries kept. A line per
/// statement run is small for a long time; read it backwards from the end if
/// one ever gets big enough to feel.
pub const HISTORY_DEPTH: usize = 200;
/// `errSecItemNotFound`. Apple's `OSStatus` values are frozen ABI, and the
/// named constant lives in `security-framework-sys`, which is not a dependency
/// here -- adding it with the exact pin this project uses everywhere would
/// fight `security-framework`'s own transitive bump of it.
const ITEM_NOT_FOUND: i32 = -25300;

/// Field order is load-bearing: TOML cannot emit a scalar after a table, so
/// every scalar has to precede `open_objects`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredProfile {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: String,
    /// libpq's spelling, so a profile file stays readable and a mode Slate
    /// stops supporting reads back as a name rather than a number. Defaulted,
    /// so profiles written before TLS existed load as `prefer` — which is what
    /// they were connecting as.
    #[serde(default)]
    pub sslmode: Option<String>,
    #[serde(default)]
    pub root_certificate: Option<String>,
    /// `db::Engine::as_str()`. Absent is a profile written before a second
    /// engine existed, and reads back as Postgres -- which is what it was
    /// connecting as.
    #[serde(default)]
    pub engine: Option<String>,
    /// The database file, for SQLite. Absent for a server engine.
    #[serde(default)]
    pub path: Option<String>,
    /// The editor's zoom. Absent is a profile written before zoom was kept, and
    /// reads back as the default -- which is what it was showing.
    #[serde(default)]
    pub editor_font_size: Option<f32>,
    /// Seconds a statement may run before the engine stops it, or 0 / absent
    /// for no limit. Absent is a profile written before the field existed, and
    /// no limit is exactly what it was running with.
    #[serde(default)]
    pub statement_timeout: Option<u32>,
    #[serde(default)]
    pub open_query: Option<String>,
    #[serde(default)]
    pub open_objects: Vec<StoredObject>,
}

/// An opened table, view or routine, stored by name rather than by content: the
/// catalog is the source of truth for what it holds, so a restored tab shows
/// today's definition and one that has been dropped simply does not come back.
///
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StoredObject {
    pub schema: String,
    pub name: String,
    #[serde(default)]
    pub routine: bool,
    /// Which tab was in front. A flag on the object rather than a pointer to
    /// it: a name can contain anything, including whatever would separate a
    /// schema from a relation in a key.
    #[serde(default)]
    pub active: bool,
}

/// The three font families in use. App-level rather than per-profile: the face
/// Slate is read in belongs to the person reading, not to the database they
/// happen to be connected to. Every field is optional, so a file written before
/// fonts were pickable reads back as the defaults -- which is what it was drawn
/// in.
#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredFonts {
    #[serde(default)]
    pub chrome: Option<String>,
    #[serde(default)]
    pub editor: Option<String>,
    #[serde(default)]
    pub grid: Option<String>,
}

/// A decoded profile file: the profiles, the id of the one that was in front,
/// and the fonts. Named because it is three things and a bare triple in the
/// signature reads as none of them.
type Restored = (Vec<StoredProfile>, Option<String>, Option<StoredFonts>);

/// Field order is load-bearing here too: `active` is a scalar, so it has to
/// precede both tables, and `fonts` is a table, so it has to precede the
/// profile table array.
#[derive(Default, Debug, PartialEq, Serialize, Deserialize)]
struct ProfileFile {
    /// The profile that was in front. An id rather than a flag on the profile,
    /// unlike [`StoredObject::active`]: an id is a validated slug, so there is
    /// nothing in one that could be mistaken for a key.
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    fonts: Option<StoredFonts>,
    #[serde(default)]
    profiles: Vec<StoredProfile>,
}

/// The profile list and the id of the one that was last in front. A missing
/// file is the first run, and reads as an empty list. Every other failure is
/// reported, a missing `HOME` included -- `save_profiles` refuses on that too,
/// and an empty list here is what the next save writes back.
pub fn load_profiles() -> Result<Restored, String> {
    let path = slate_directory()?.join(PROFILES_FILE);
    // A file we could not read is not renamed: nothing is recovered by moving
    // it, so the overwrite hazard below technically remains. A directory we
    // cannot read is one we almost certainly cannot write either.
    let Some(text) = read_file(&path)? else {
        return Ok((Vec::new(), None, None));
    };
    decode_profiles(&text).map_err(|error| {
        // The next save rewrites this path, so moving the unparsable file aside
        // first is what keeps a profile list a single bad byte would cost.
        let kept = path.with_file_name(format!("{PROFILES_FILE}.broken"));
        match fs::rename(&path, &kept) {
            Ok(()) => format!(
                "Could not read {} as TOML, and it has been kept as {}: {error}",
                path.display(),
                kept.display()
            ),
            Err(rename_error) => format!(
                "Could not read {} as TOML, and it could not be moved aside ({rename_error}): {error}",
                path.display()
            ),
        }
    })
}

fn decode_profiles(text: &str) -> Result<Restored, String> {
    toml::from_str::<ProfileFile>(text)
        .map(|file| (file.profiles, file.active, file.fonts))
        .map_err(|error| error.to_string())
}

pub fn save_profiles(
    profiles: &[StoredProfile],
    active: Option<&str>,
    fonts: &StoredFonts,
) -> Result<(), String> {
    let text = toml::to_string_pretty(&ProfileFile {
        active: active.map(str::to_string),
        fonts: Some(fonts.clone()),
        profiles: profiles.to_vec(),
    })
    .map_err(|error| format!("Could not encode the profile list: {error}"))?;
    write_file(&slate_directory()?.join(PROFILES_FILE), &text)
}

pub fn profile_id(name: &str, existing: &[String]) -> String {
    let slug: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let parts = slug
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let base = if parts.is_empty() {
        "profile".to_string()
    } else {
        parts.join("-")
    };

    let mut candidate = base.clone();
    let mut suffix = 2;
    while existing.iter().any(|id| id == &candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// `Ok(None)` only for a keychain that holds no item for this profile. A denied
/// prompt, a locked keychain and a secret that is not text are failures: a
/// blank password is valid, so none of them can be inferred from the connect
/// attempt that would follow.
pub fn password(profile_id: &str) -> Result<Option<String>, String> {
    let bytes = match passwords::generic_password(PasswordOptions::new_generic_password(
        KEYCHAIN_SERVICE,
        profile_id,
    )) {
        Ok(bytes) => bytes,
        Err(error) if error.code() == ITEM_NOT_FOUND => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Could not read the password from the keychain: {error}"
            ));
        }
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| format!("The keychain password is not valid text: {error}"))
}

pub fn set_password(profile_id: &str, password: &str) -> Result<(), String> {
    passwords::set_generic_password(KEYCHAIN_SERVICE, profile_id, password.as_bytes())
        .map_err(|error| format!("Could not save the password to the keychain: {error}"))
}

pub fn delete_password(profile_id: &str) {
    let _ = passwords::delete_generic_password(KEYCHAIN_SERVICE, profile_id);
}

pub fn saved_queries(profile_id: &str) -> Vec<String> {
    let Ok(directory) = query_directory(profile_id) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut names = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry
                .file_name()
                .to_str()?
                .strip_suffix(".sql")?
                .to_string();
            validate_query_name(&name).ok()?;
            Some(name)
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}

pub fn read_query(profile_id: &str, name: &str) -> Result<Option<String>, String> {
    read_file(&query_path(profile_id, name)?)
}

pub fn write_query(profile_id: &str, name: &str, sql: &str) -> Result<(), String> {
    write_file(&query_path(profile_id, name)?, sql)
}

/// Every query a profile saved, on its way out with the profile.
///
/// `profile_id` derives from the name, so a profile recreated under the name of
/// a removed one is handed the same id -- and would open a dead profile's
/// queries as its own. Leaving the directory behind is what makes that happen.
pub fn delete_queries(profile_id: &str) -> Result<(), String> {
    let directory = query_directory(profile_id)?;
    match fs::remove_dir_all(&directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not delete {}: {error}", directory.display())),
    }
}

pub fn delete_query(profile_id: &str, name: &str) -> Result<(), String> {
    let path = query_path(profile_id, name)?;
    fs::remove_file(&path).map_err(|error| format!("Could not delete {}: {error}", path.display()))
}

pub fn validate_query_name(name: &str) -> Result<(), String> {
    match unsafe_component(name) {
        Some(reason) => Err(format!("Query name {reason}.")),
        None => Ok(()),
    }
}

pub fn read_scratch(profile_id: &str) -> Result<Option<String>, String> {
    read_file(&query_directory(profile_id)?.join(SCRATCH_FILE))
}

pub fn write_scratch(profile_id: &str, sql: &str) -> Result<(), String> {
    write_file(&query_directory(profile_id)?.join(SCRATCH_FILE), sql)
}

/// One statement on its way into a profile's history, newest at the end.
///
/// A JSON string per line rather than the SQL itself: a statement holds
/// newlines, semicolons and comments, so there is no separator to put between
/// two of them that is not also SQL. Appended rather than rewritten, so a run
/// costs one write and no history can be lost to a rewrite that failed
/// halfway.
pub fn append_history(profile_id: &str, sql: &str) -> Result<(), String> {
    let directory = query_directory(profile_id)?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Could not create {}: {error}", directory.display()))?;
    let path = directory.join(HISTORY_FILE);
    let line = serde_json::to_string(sql)
        .map_err(|error| format!("Could not encode the statement: {error}"))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    secure(&path)?;
    writeln!(file, "{line}").map_err(|error| format!("Could not write {}: {error}", path.display()))
}

/// What this profile has run, newest first and each statement once. A missing
/// or unreadable file reads as no history: there is nothing here that the user
/// wrote and cannot get back another way.
pub fn history(profile_id: &str) -> Vec<String> {
    let Ok(directory) = query_directory(profile_id) else {
        return Vec::new();
    };
    match fs::read_to_string(directory.join(HISTORY_FILE)) {
        Ok(text) => decode_history(&text),
        Err(_) => Vec::new(),
    }
}

/// A line that will not decode is skipped rather than ending the read: the file
/// is appended to on every run, and the half-written last line a crash leaves
/// behind is not a reason to lose everything before it.
fn decode_history(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .filter(|sql| seen.insert(sql.clone()))
        .take(HISTORY_DEPTH)
        .collect()
}

fn unsafe_component(value: &str) -> Option<&'static str> {
    if value.trim().is_empty() {
        Some("is empty")
    } else if value.contains(['/', '\\']) {
        Some("contains a path separator")
    } else if value.contains('\0') {
        Some("contains a NUL character")
    } else if value.starts_with('.') {
        Some("starts with a dot")
    } else {
        None
    }
}

fn slate_directory() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .ok_or_else(|| "HOME is not set.".to_string())?;
    Ok(PathBuf::from(home).join("Library/Application Support/Slate"))
}

fn query_directory(profile_id: &str) -> Result<PathBuf, String> {
    if let Some(reason) = unsafe_component(profile_id) {
        return Err(format!("Profile id {reason}."));
    }
    Ok(slate_directory()?.join("queries").join(profile_id))
}

fn query_path(profile_id: &str, name: &str) -> Result<PathBuf, String> {
    validate_query_name(name)?;
    Ok(query_directory(profile_id)?.join(format!("{name}.sql")))
}

/// `Ok(None)` for a file that is not there, which every caller has a sensible
/// answer for. A file that exists and could not be read does not get the same
/// answer -- that is the one that sends a person looking for lost work.
fn read_file(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, contents)
        .map_err(|error| format!("Could not write {}: {error}", temporary.display()))?;
    // Set before the rename, not after: the rename is what makes this the file
    // at `path`, so a mode applied afterward would leave it world-readable for
    // however long the two steps are apart.
    secure(&temporary)?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("Could not replace {}: {error}", path.display()))
}

/// Connection settings live in these files -- host, port, user, database --
/// so `0600` holds regardless of whether `path` is being created or replaced.
/// `OpenOptions::mode` only sets this at creation, which is not enough for a
/// file that already existed with looser permissions from before this rule.
fn secure(path: &Path) -> Result<(), String> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not set permissions on {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn a_profile_survives_the_round_trip_through_toml() {
        // TOML refuses a scalar written after a table, so the open-object list
        // has to stay the last field -- and that is invisible until it fails.
        let profile = StoredProfile {
            id: "dev".into(),
            name: "Dev".into(),
            host: "127.0.0.1".into(),
            port: Some(5432),
            database: "slate_dev".into(),
            user: "slate".into(),
            sslmode: Some("verify-full".into()),
            root_certificate: Some("/etc/ssl/rds.pem".into()),
            engine: Some("postgres".into()),
            path: None,
            editor_font_size: Some(16.0),
            statement_timeout: Some(30),
            open_query: Some("daily".into()),
            open_objects: vec![
                StoredObject {
                    schema: "public".into(),
                    name: "accounts".into(),
                    routine: false,
                    active: true,
                },
                StoredObject {
                    schema: "public".into(),
                    name: "total(integer)".into(),
                    routine: true,
                    active: false,
                },
            ],
        };
        let file = ProfileFile {
            fonts: None,
            active: Some("dev".into()),
            profiles: vec![profile.clone()],
        };

        let text = toml::to_string_pretty(&file).expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
        assert_eq!(decoded.active.as_deref(), Some("dev"));
    }

    #[test]
    fn a_profile_written_before_a_field_existed_still_loads() {
        // Every field added after the first release is `serde(default)`, and this
        // is the file already on disk for anyone who has run Slate before. A
        // decode error here reads as "no profiles", which is what the next save
        // would then write back.
        let (profiles, active, ..) = decode_profiles(
            "\
[[profiles]]
id = \"slate-dev\"
name = \"slate_dev\"
host = \"127.0.0.1\"
port = 55432
database = \"slate_dev\"
user = \"slate\"
sslmode = \"prefer\"
open_objects = []
",
        )
        .expect("a profile predating the optional fields must load");

        assert_eq!(active, None);
        let [profile] = &profiles[..] else {
            panic!("expected exactly one profile, got {}", profiles.len());
        };
        assert_eq!(profile.editor_font_size, None);
        assert_eq!(profile.root_certificate, None);
        assert_eq!(profile.open_query, None);
        // No limit, which is what it was running with.
        assert_eq!(profile.statement_timeout, None);
    }

    #[test]
    fn a_sqlite_profile_survives_the_round_trip_through_toml() {
        // SQLite has no host, port, user or TLS -- a profile for it writes
        // those fields blank rather than omitting them, since `StoredProfile`
        // stays one shape for every engine.
        let profile = StoredProfile {
            id: "local".into(),
            name: "Local".into(),
            host: String::new(),
            port: None,
            database: String::new(),
            user: String::new(),
            sslmode: None,
            root_certificate: None,
            engine: Some("sqlite".into()),
            path: Some("/Users/dev/slate_dev.db".into()),
            editor_font_size: Some(14.0),
            statement_timeout: None,
            open_query: None,
            open_objects: vec![StoredObject {
                schema: "main".into(),
                name: "accounts".into(),
                routine: false,
                active: true,
            }],
        };
        let file = ProfileFile {
            active: None,
            fonts: None,
            profiles: vec![profile.clone()],
        };

        let text = toml::to_string_pretty(&file).expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
    }

    #[test]
    fn a_profile_written_before_engine_existed_still_loads() {
        // Every profile on disk before a second engine existed has no `engine`
        // key at all, and must load as Postgres -- which is what it was
        // connecting as -- rather than fail to decode.
        let (profiles, ..) = decode_profiles(
            "\
[[profiles]]
id = \"slate-dev\"
name = \"slate_dev\"
host = \"127.0.0.1\"
port = 55432
database = \"slate_dev\"
user = \"slate\"
sslmode = \"prefer\"
open_objects = []
",
        )
        .expect("a profile predating engine must load");

        let [profile] = &profiles[..] else {
            panic!("expected exactly one profile, got {}", profiles.len());
        };
        assert_eq!(profile.engine, None);
        assert_eq!(profile.path, None);
    }

    #[test]
    fn engine_and_path_precede_the_open_objects_table() {
        // TOML cannot emit a scalar after a table, so `engine` and `path` have
        // to sit before `open_objects` in the struct. This is the check that
        // would actually fail if someone reordered it.
        let profile = StoredProfile {
            id: "local".into(),
            name: "Local".into(),
            host: String::new(),
            port: None,
            database: String::new(),
            user: String::new(),
            sslmode: None,
            root_certificate: None,
            engine: Some("sqlite".into()),
            path: Some("/tmp/dev.sqlite".into()),
            editor_font_size: None,
            statement_timeout: Some(30),
            open_query: None,
            open_objects: vec![StoredObject {
                schema: "main".into(),
                name: "accounts".into(),
                routine: false,
                active: true,
            }],
        };
        let text = toml::to_string_pretty(&ProfileFile {
            active: None,
            fonts: None,
            profiles: vec![profile],
        })
        .expect("profile must encode");

        let engine_at = text.find("engine = ").expect("engine must be written");
        let timeout_at = text
            .find("statement_timeout = ")
            .expect("statement_timeout must be written");
        let path_at = text.find("path = ").expect("path must be written");
        let open_objects_at = text
            .find("[[profiles.open_objects]]")
            .expect("open_objects must be written as a table");

        assert!(
            engine_at < open_objects_at,
            "engine after open_objects:\n{text}"
        );
        assert!(
            path_at < open_objects_at,
            "path after open_objects:\n{text}"
        );
        assert!(
            timeout_at < open_objects_at,
            "statement_timeout after open_objects:\n{text}"
        );
    }

    #[test]
    fn an_unparsable_profile_file_is_an_error_rather_than_an_empty_list() {
        // The empty list is what the next save writes back, so a parse error
        // that reads as "no profiles" is a parse error that deletes them.
        assert!(decode_profiles("host = ").is_err());
        assert_eq!(decode_profiles(""), Ok((Vec::new(), None, None)));
    }

    #[test]
    fn profile_ids_are_safe_and_stable() {
        assert_eq!(profile_id("Prod (EU West)", &[]), "prod-eu-west");
        assert_eq!(profile_id("///", &[]), "profile");
        assert_eq!(profile_id("Prod", &ids(&["prod", "prod-2"])), "prod-3");
    }

    #[test]
    fn unsafe_query_names_are_rejected() {
        for name in [
            "",
            "   ",
            ".",
            "..",
            ".scratch",
            "../secrets",
            "a/b",
            "a\\b",
            "a\0b",
        ] {
            assert!(validate_query_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn a_history_file_reads_back_newest_first_and_each_statement_once() {
        // The multi-line statement is the point of the encoding: a raw-SQL file
        // has no separator between two statements that is not also SQL.
        let text = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string("SELECT 1").unwrap(),
            serde_json::to_string("SELECT\n  *\nFROM accounts; -- all").unwrap(),
            serde_json::to_string("SELECT 1").unwrap(),
        );

        assert_eq!(
            decode_history(&text),
            ["SELECT 1", "SELECT\n  *\nFROM accounts; -- all"]
        );
    }

    #[test]
    fn a_half_written_line_does_not_take_the_history_with_it() {
        let text = format!("{}\n\"SELECT 2", serde_json::to_string("SELECT 1").unwrap());

        assert_eq!(decode_history(&text), ["SELECT 1"]);
    }

    #[test]
    fn a_written_config_file_is_readable_only_by_its_owner() {
        // Profiles, saved queries and the scratch buffer all hold connection
        // settings and go through this one function, so this is the one place
        // that has to prove the permission rather than every caller.
        let path = std::env::temp_dir().join("slate-store-permissions-test.toml");
        let _ = fs::remove_file(&path);

        write_file(&path, "host = \"example\"").expect("file must write");

        let mode = fs::metadata(&path)
            .expect("file must exist")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn ordinary_query_names_are_accepted() {
        for name in ["daily report", "v1.2 counts", "accounts", "-x-"] {
            assert!(validate_query_name(name).is_ok(), "{name:?}");
        }
    }
}
