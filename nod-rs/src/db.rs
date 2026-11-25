use crate::util::{iso_now, uuid};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Store {
    connection: Mutex<Connection>,
    pub directory: PathBuf,
}

impl Store {
    pub fn open() -> Result<Self, String> {
        Self::open_at(data_directory())
    }

    pub fn open_at(directory: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&directory)
            .map_err(|error| format!("Could not create {}: {error}", directory.display()))?;
        let path = directory.join("nod.sqlite3");
        let connection = Connection::open(&path)
            .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
        init_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            directory,
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, String> {
        let connection = Connection::open_in_memory()
            .map_err(|error| format!("Could not open in-memory database: {error}"))?;
        init_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
            directory: std::env::temp_dir(),
        })
    }
}

fn init_schema(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA cache_size=-4000;
             PRAGMA temp_store=MEMORY;
             PRAGMA mmap_size=0;
             CREATE TABLE IF NOT EXISTS docs(
                collection TEXT NOT NULL,
                id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                body TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                PRIMARY KEY(collection,id)
             );
             CREATE INDEX IF NOT EXISTS docs_order ON docs(collection,seq);
             CREATE TABLE IF NOT EXISTS settings(
                key TEXT PRIMARY KEY,
                body TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS counters(
                name TEXT PRIMARY KEY,
                value INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS blobs(
                id TEXT PRIMARY KEY,
                body BLOB NOT NULL,
                created_at TEXT NOT NULL
             );",
        )
        .map_err(|error| format!("Could not prepare the database: {error}"))
}

impl Store {

    fn with<T>(&self, run: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "The local database is unavailable.".to_string())?;
        run(&connection).map_err(|error| format!("The local database failed: {error}"))
    }

    pub fn put(&self, collection: &str, id: &str, body: &Value) -> Result<(), String> {
        let serialized = body.to_string();
        let updated = iso_now();
        self.with(|connection| {
            let exists: bool = connection
                .query_row(
                    "SELECT 1 FROM docs WHERE collection=?1 AND id=?2",
                    params![collection, id],
                    |_| Ok(()),
                )
                .optional()
                .map(|value| value.is_some())
                .unwrap_or(false);
            if exists {
                connection.execute(
                    "UPDATE docs SET body=?1, updated_at=?2
                     WHERE collection=?3 AND id=?4",
                    params![serialized, updated, collection, id],
                )?;
            } else {
                let seq: i64 = connection.query_row(
                    "SELECT COALESCE(MAX(seq),0)+1 FROM docs",
                    [],
                    |row| row.get(0),
                )?;
                connection.execute(
                    "INSERT INTO docs(collection,id,seq,body,updated_at)
                     VALUES(?1,?2,?3,?4,?5)",
                    params![collection, id, seq, serialized, updated],
                )?;
            }
            Ok(())
        })
    }

    pub fn get(&self, collection: &str, id: &str) -> Option<Value> {
        let result: Result<Option<String>, String> = self.with(|connection| {
            connection
                .query_row(
                    "SELECT body FROM docs WHERE collection=?1 AND id=?2",
                    params![collection, id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
        });
        result
            .ok()
            .flatten()
            .and_then(|body| serde_json::from_str(&body).ok())
    }

    pub fn exists(&self, collection: &str, id: &str) -> bool {
        self.get(collection, id).is_some()
    }

    pub fn all(&self, collection: &str) -> Vec<Value> {
        let result: Result<Vec<String>, String> = self.with(|connection| {
            let mut statement =
                connection.prepare("SELECT body FROM docs WHERE collection=?1 ORDER BY seq")?;
            let rows = statement.query_map(params![collection], |row| row.get::<_, String>(0))?;
            rows.collect()
        });
        result
            .unwrap_or_default()
            .into_iter()
            .filter_map(|body| serde_json::from_str(&body).ok())
            .collect()
    }

    pub fn delete(&self, collection: &str, id: &str) -> bool {
        self.with(|connection| {
            connection
                .execute(
                    "DELETE FROM docs WHERE collection=?1 AND id=?2",
                    params![collection, id],
                )
                .map(|count| count > 0)
        })
        .unwrap_or(false)
    }

    pub fn clear(&self, collection: &str) {
        let _ = self.with(|connection| {
            connection.execute("DELETE FROM docs WHERE collection=?1", params![collection])
        });
    }

    pub fn patch(&self, collection: &str, id: &str, patch: &Value) -> Option<Value> {
        let mut current = self.get(collection, id)?;
        merge(&mut current, patch);
        let _ = self.put(collection, id, &current);
        Some(current)
    }

    pub fn count(&self, collection: &str) -> usize {
        let result: Result<i64, String> = self.with(|connection| {
            connection.query_row(
                "SELECT COUNT(*) FROM docs WHERE collection=?1",
                params![collection],
                |row| row.get(0),
            )
        });
        result.unwrap_or(0).max(0) as usize
    }

    pub fn setting(&self, key: &str) -> Option<Value> {
        let result: Result<Option<String>, String> = self.with(|connection| {
            connection
                .query_row(
                    "SELECT body FROM settings WHERE key=?1",
                    params![key],
                    |row| row.get::<_, String>(0),
                )
                .optional()
        });
        result
            .ok()
            .flatten()
            .and_then(|body| serde_json::from_str(&body).ok())
    }

    pub fn set_setting(&self, key: &str, value: &Value) {
        let serialized = value.to_string();
        let _ = self.with(|connection| {
            connection.execute(
                "INSERT INTO settings(key,body) VALUES(?1,?2)
                 ON CONFLICT(key) DO UPDATE SET body=?2",
                params![key, serialized],
            )
        });
    }

    pub fn update_setting(&self, key: &str, patch: &Value) -> Value {
        let mut current = self.setting(key).unwrap_or_else(|| json!({}));
        merge(&mut current, patch);
        self.set_setting(key, &current);
        current
    }

    #[allow(dead_code)]
    pub fn put_blob(&self, bytes: &[u8]) -> Result<String, String> {
        let id = uuid();
        let created = iso_now();
        self.with(|connection| {
            connection.execute(
                "INSERT INTO blobs(id,body,created_at) VALUES(?1,?2,?3)",
                params![id, bytes, created],
            )
        })?;
        Ok(id)
    }

    #[allow(dead_code)]
    pub fn get_blob(&self, id: &str) -> Option<Vec<u8>> {
        self.with(|connection| {
            connection
                .query_row(
                    "SELECT body FROM blobs WHERE id=?1",
                    params![id],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
        })
        .ok()
        .flatten()
    }

    #[allow(dead_code)]
    pub fn delete_blob(&self, id: &str) {
        let _ = self.with(|connection| {
            connection.execute("DELETE FROM blobs WHERE id=?1", params![id])
        });
    }

    pub fn document_path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }
}

pub fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    target.remove(key);
                } else {
                    merge(target.entry(key.clone()).or_insert(Value::Null), value);
                }
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

pub fn data_directory() -> PathBuf {
    if let Ok(custom) = std::env::var("NOD_DATA_DIR")
        && !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
    for variable in ["APPDATA", "LOCALAPPDATA"] {
        if let Ok(root) = std::env::var(variable)
            && !root.trim().is_empty() {
                return PathBuf::from(root).join("Nod");
            }
    }
    PathBuf::from("nod-data")
}
