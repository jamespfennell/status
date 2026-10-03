//! A simple database for persisting data across runs of the agent.

use std::collections::{BTreeMap, HashMap};
use std::{fs, sync};

pub trait DB: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;

    fn set(&self, key: String, value: String);
}

pub fn get_typed<T: serde::de::DeserializeOwned>(
    db: &dyn DB,
    key: &str,
) -> serde_json::Result<Option<T>> {
    let Some(raw) = db.get(key) else {
        return Ok(None);
    };
    let t: T = serde_json::from_str(&raw)?;
    Ok(Some(t))
}

pub fn set_typed<T: serde::Serialize>(
    db: &dyn DB,
    key: String,
    value: &T,
) -> serde_json::Result<()> {
    let raw = serde_json::to_string_pretty(&value)?;
    db.set(key, raw);
    Ok(())
}

pub fn new_in_memory_db() -> impl DB {
    InMemoryDB::default()
}

#[derive(Default)]
struct InMemoryDB {
    m: sync::Mutex<HashMap<String, String>>,
}

impl DB for InMemoryDB {
    fn get(&self, key: &str) -> Option<String> {
        self.m.lock().unwrap().get(key).cloned()
    }
    fn set(&self, key: String, value: String) {
        self.m.lock().unwrap().insert(key, value);
    }
}

pub fn new_on_disk_db(path: std::path::PathBuf) -> Result<impl DB, OnDiskDBPathError> {
    OnDiskDB::load(path)
}

/// Error when loading an on-disk database.
#[derive(Debug)]
pub enum OnDiskDBPathError {
    MissingFileName,
    IO(std::io::Error),
}

impl std::fmt::Display for OnDiskDBPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use OnDiskDBPathError::*;
        match self {
            MissingFileName => write!(f, "path points to a directory and not a file"),
            IO(error) => write!(f, "IO error: {error}"),
        }
    }
}

struct OnDiskDB {
    path: std::path::PathBuf,
    tmp_path: std::path::PathBuf,
    m: sync::Mutex<BTreeMap<String, String>>,
}

impl OnDiskDB {
    fn load(path: std::path::PathBuf) -> Result<Self, OnDiskDBPathError> {
        let tmp_path = {
            let Some(file_name) = path.file_name() else {
                return Err(OnDiskDBPathError::MissingFileName);
            };
            let mut file_name = file_name.to_os_string();
            file_name.push(".tmp");
            let mut tmp_path = path.clone();
            tmp_path.set_file_name(file_name);
            tmp_path
        };
        let m: BTreeMap<String, String> = match fs::read_to_string(&path) {
            Ok(data) => parse_db_file(&data),
            Err(err) => {
                if err.kind() == std::io::ErrorKind::NotFound {
                    eprintln!(
                        "Database file {} doesn't exist; initializing new database",
                        path.display()
                    );
                    Default::default()
                } else {
                    return Err(OnDiskDBPathError::IO(err));
                }
            }
        };
        Ok(OnDiskDB {
            path,
            tmp_path,
            m: sync::Mutex::new(m),
        })
    }
}

fn parse_db_file(s: &str) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = Default::default();
    let mut offset = 0_usize;
    let mut current_key: Option<(String, usize)> = None;
    let mut iter = s.split_inclusive('\n');
    loop {
        let line_or = iter.next();
        let flush_or = match line_or {
            None => current_key.take(),
            Some(line) => match line.strip_prefix("// key=") {
                None => None,
                Some(new_key) => {
                    let old = current_key.take();
                    current_key = Some((new_key.trim_end().to_string(), offset + line.len()));
                    old
                }
            },
        };
        if let Some((key, val_start)) = flush_or {
            let val = s[val_start..offset].trim().to_string();
            m.insert(key, val);
        };
        match line_or {
            None => break,
            Some(line) => {
                offset += line.len();
            }
        }
    }
    m
}

impl DB for OnDiskDB {
    fn get(&self, key: &str) -> Option<String> {
        self.m.lock().unwrap().get(key).cloned()
    }
    fn set(&self, key: String, value: String) {
        let mut m = self.m.lock().unwrap();
        use std::collections::btree_map::Entry;
        match m.entry(key) {
            Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(value);
            }
            Entry::Occupied(mut occupied_entry) => {
                if occupied_entry.get().eq(&value) {
                    // Value hasn't changed.
                    // Exit early without writing change to disk.
                    return;
                }
                *occupied_entry.get_mut() = value;
            }
        }
        let mut buf = "// This is a status agent database file.\n// https://github.com/jamespfennell/status\n//\n".to_string();
        for (k, v) in &*m {
            use std::fmt::Write;
            writeln!(&mut buf, "// key={k}\n{v}").unwrap();
        }
        match std::fs::write(&self.tmp_path, &buf) {
            Ok(()) => (),
            Err(err) => {
                println!(
                    "Failed to write database to {}: {err}",
                    self.tmp_path.display()
                );
                return;
            }
        };
        match std::fs::rename(&self.tmp_path, &self.path) {
            Ok(()) => (),
            Err(err) => {
                println!(
                    "Failed to finalize database from {} to {}: {err}",
                    self.tmp_path.display(),
                    self.path.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
    struct TestStruct {
        field_1: i32,
        field_2: Vec<bool>,
    }
    #[test]
    fn on_disk_db() {
        let dir = std::env::temp_dir();
        let path = dir.join("status_on_disk_db.json");
        if let Err(err) = std::fs::remove_file(path.clone()) {
            eprintln!("failed to delete file: {err}");
        }

        let db = new_on_disk_db(path.clone()).unwrap();
        let key_1 = "string".to_string();
        let val_1 = "value".to_string();
        let key_2 = "struct".to_string();
        let val_2 = TestStruct {
            field_1: 32,
            field_2: vec![true, true, false],
        };

        assert_eq!(get_typed::<String>(&db, &key_1).unwrap(), None);
        set_typed(&db, key_1.clone(), &val_1).unwrap();
        set_typed(&db, key_2.clone(), &val_2).unwrap();

        assert_eq!(get_typed(&db, &key_1).unwrap(), Some(val_1.clone()));
        assert_eq!(get_typed(&db, &key_2).unwrap(), Some(val_2.clone()));

        let db = new_on_disk_db(path).unwrap();
        assert_eq!(get_typed(&db, &key_1).unwrap(), Some(val_1));
        assert_eq!(get_typed(&db, &key_2).unwrap(), Some(val_2));
    }
}
