use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use security_framework::passwords::{self, PasswordOptions};
use serde::{Deserialize, Serialize};

const PROFILES_FILE: &str = "profiles.toml";
const KEYCHAIN_SERVICE: &str = "Slate";
const SCRATCH_FILE: &str = ".scratch.sql";
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

#[derive(Default, Debug, PartialEq, Serialize, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profiles: Vec<StoredProfile>,
}

/// A missing file is the first run, and reads as an empty list. Every other
/// failure is reported, a missing `HOME` included -- `save_profiles` refuses on
/// that too, and an empty list here is what the next save writes back.
pub fn load_profiles() -> Result<Vec<StoredProfile>, String> {
    let path = slate_directory()?.join(PROFILES_FILE);
    // A file we could not read is not renamed: nothing is recovered by moving
    // it, so the overwrite hazard below technically remains. A directory we
    // cannot read is one we almost certainly cannot write either.
    let Some(text) = read_file(&path)? else {
        return Ok(Vec::new());
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

fn decode_profiles(text: &str) -> Result<Vec<StoredProfile>, String> {
    toml::from_str::<ProfileFile>(text)
        .map(|file| file.profiles)
        .map_err(|error| error.to_string())
}

pub fn save_profiles(profiles: &[StoredProfile]) -> Result<(), String> {
    let text = toml::to_string_pretty(&ProfileFile {
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
            let name = entry.file_name().to_str()?.strip_suffix(".sql")?.to_string();
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
    fs::rename(&temporary, path)
        .map_err(|error| format!("Could not replace {}: {error}", path.display()))
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
            profiles: vec![profile.clone()],
        };

        let text = toml::to_string_pretty(&file).expect("profiles must encode");
        let decoded: ProfileFile = toml::from_str(&text).expect("profiles must decode");

        assert_eq!(decoded.profiles, vec![profile]);
    }

    #[test]
    fn an_unparsable_profile_file_is_an_error_rather_than_an_empty_list() {
        // The empty list is what the next save writes back, so a parse error
        // that reads as "no profiles" is a parse error that deletes them.
        assert!(decode_profiles("host = ").is_err());
        assert_eq!(decode_profiles(""), Ok(Vec::new()));
    }

    #[test]
    fn profile_ids_are_safe_and_stable() {
        assert_eq!(profile_id("Prod (EU West)", &[]), "prod-eu-west");
        assert_eq!(profile_id("///", &[]), "profile");
        assert_eq!(profile_id("Prod", &ids(&["prod", "prod-2"])), "prod-3");
    }

    #[test]
    fn unsafe_query_names_are_rejected() {
        for name in ["", "   ", ".", "..", ".scratch", "../secrets", "a/b", "a\\b", "a\0b"] {
            assert!(validate_query_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn ordinary_query_names_are_accepted() {
        for name in ["daily report", "v1.2 counts", "accounts", "-x-"] {
            assert!(validate_query_name(name).is_ok(), "{name:?}");
        }
    }
}
