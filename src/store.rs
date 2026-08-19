use std::fs;
use std::path::{Path, PathBuf};

use security_framework::passwords::{self, PasswordOptions};
use serde::{Deserialize, Serialize};

const PROFILES_FILE: &str = "profiles.toml";
const KEYCHAIN_SERVICE: &str = "Slate";
const SCRATCH_FILE: &str = ".scratch.sql";

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
/// A buffer the user edited is the exception, because that is theirs and the
/// catalog cannot regenerate it.
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
    #[serde(default)]
    pub sql: Option<String>,
}

#[derive(Default, Debug, PartialEq, Serialize, Deserialize)]
struct ProfileFile {
    #[serde(default)]
    profiles: Vec<StoredProfile>,
}

pub fn load_profiles() -> Vec<StoredProfile> {
    let Ok(directory) = slate_directory() else {
        return Vec::new();
    };
    fs::read_to_string(directory.join(PROFILES_FILE))
        .ok()
        .and_then(|text| toml::from_str::<ProfileFile>(&text).ok())
        .map(|file| file.profiles)
        .unwrap_or_default()
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

pub fn password(profile_id: &str) -> Option<String> {
    let bytes = passwords::generic_password(PasswordOptions::new_generic_password(
        KEYCHAIN_SERVICE,
        profile_id,
    ))
    .ok()?;
    String::from_utf8(bytes).ok()
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

pub fn read_query(profile_id: &str, name: &str) -> Option<String> {
    fs::read_to_string(query_path(profile_id, name).ok()?).ok()
}

pub fn write_query(profile_id: &str, name: &str, sql: &str) -> Result<(), String> {
    write_file(&query_path(profile_id, name)?, sql)
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

pub fn read_scratch(profile_id: &str) -> Option<String> {
    fs::read_to_string(query_directory(profile_id).ok()?.join(SCRATCH_FILE)).ok()
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
                    sql: Some("select 1".into()),
                },
                StoredObject {
                    schema: "public".into(),
                    name: "total(integer)".into(),
                    routine: true,
                    active: false,
                    sql: None,
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
