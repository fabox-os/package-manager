use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("The update history database could not be used: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Could not create the data folder: {0}")]
    Io(#[from] std::io::Error),
    #[error("The database was created by a newer version of Package Manager (schema {found}, this version supports {supported}).")]
    TooNew { found: u32, supported: u32 },
    #[error("The database is busy. Try again.")]
    Poisoned,
}

/// Forward-only migrations. `PRAGMA user_version` records how many have run.
/// Never edit an existing entry: append a new one.
const MIGRATIONS: &[&str] = &[
    // 1: update history (also records where each update came from)
    "CREATE TABLE history (
         id           INTEGER PRIMARY KEY AUTOINCREMENT,
         timestamp    INTEGER NOT NULL,
         app_id       TEXT NOT NULL,
         app_name     TEXT NOT NULL,
         from_version TEXT NOT NULL,
         to_version   TEXT NOT NULL,
         source       TEXT NOT NULL,
         provenance   TEXT NOT NULL,
         success      INTEGER NOT NULL,
         message      TEXT
     );
     CREATE INDEX history_app ON history(app_id, timestamp);",
    // 2: apps the user linked to a GitHub repository
    "CREATE TABLE tracked_apps (
         package_name  TEXT PRIMARY KEY,
         github_repo   TEXT NOT NULL,
         asset_pattern TEXT NOT NULL,
         is_preset     INTEGER NOT NULL,
         linked_at     INTEGER NOT NULL
     );",
    // 3: signing keys were an experiment that was removed; drop what early builds created
    "DROP TABLE IF EXISTS pinned_keys;",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewHistory {
    pub app_id: String,
    pub app_name: String,
    pub from_version: String,
    pub to_version: String,
    /// Badge, e.g. `APT`.
    pub source: String,
    /// e.g. `Ubuntu noble/main`.
    pub provenance: String,
    pub success: bool,
    pub message: Option<String>,
}

/// A package linked to a GitHub repository (`owner/repo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedRecord {
    pub package_name: String,
    pub github_repo: String,
    pub asset_pattern: String,
    pub is_preset: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: i64,
    /// Seconds since the Unix epoch.
    pub timestamp: i64,
    pub entry: NewHistory,
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Cheap to clone; calls block, so run them with `spawn_blocking` from async code.
#[derive(Clone)]
pub struct Database {
    conn: Arc<Mutex<Connection>>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, DbError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, DbError> {
        let found: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let supported = MIGRATIONS.len() as u32;
        if found > supported {
            return Err(DbError::TooNew { found, supported });
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
            // One transaction per migration: a failure leaves the previous version intact.
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn schema_version(&self) -> Result<u32, DbError> {
        let conn = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    pub fn add_history(&self, e: &NewHistory) -> Result<(), DbError> {
        let conn = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        conn.execute(
            "INSERT INTO history (timestamp, app_id, app_name, from_version, to_version, source, provenance, success, message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![now_secs(), e.app_id, e.app_name, e.from_version, e.to_version, e.source, e.provenance, e.success, e.message],
        )?;
        Ok(())
    }

    /// Links a package to a repo, replacing any earlier link for the same package.
    pub fn upsert_tracked(&self, t: &TrackedRecord) -> Result<(), DbError> {
        let conn = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        conn.execute(
            "INSERT INTO tracked_apps (package_name, github_repo, asset_pattern, is_preset, linked_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(package_name) DO UPDATE SET
                 github_repo = excluded.github_repo,
                 asset_pattern = excluded.asset_pattern,
                 is_preset = excluded.is_preset,
                 linked_at = excluded.linked_at",
            params![t.package_name, t.github_repo, t.asset_pattern, t.is_preset, now_secs()],
        )?;
        Ok(())
    }

    pub fn list_tracked(&self) -> Result<Vec<TrackedRecord>, DbError> {
        let conn = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT package_name, github_repo, asset_pattern, is_preset FROM tracked_apps ORDER BY package_name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(TrackedRecord {
                package_name: r.get(0)?,
                github_repo: r.get(1)?,
                asset_pattern: r.get(2)?,
                is_preset: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Returns whether a link existed.
    pub fn remove_tracked(&self, package_name: &str) -> Result<bool, DbError> {
        let conn = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        Ok(conn.execute(
            "DELETE FROM tracked_apps WHERE package_name = ?1",
            [package_name],
        )? > 0)
    }

    /// Newest first.
    pub fn recent_history(&self, limit: u32) -> Result<Vec<HistoryEntry>, DbError> {
        let conn = self.conn.lock().map_err(|_| DbError::Poisoned)?;
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, app_id, app_name, from_version, to_version, source, provenance, success, message
             FROM history ORDER BY timestamp DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(HistoryEntry {
                id: r.get(0)?,
                timestamp: r.get(1)?,
                entry: NewHistory {
                    app_id: r.get(2)?,
                    app_name: r.get(3)?,
                    from_version: r.get(4)?,
                    to_version: r.get(5)?,
                    source: r.get(6)?,
                    provenance: r.get(7)?,
                    success: r.get(8)?,
                    message: r.get(9)?,
                },
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, ok: bool) -> NewHistory {
        NewHistory {
            app_id: format!("apt:{name}"),
            app_name: name.into(),
            from_version: "1.0".into(),
            to_version: "2.0".into(),
            source: "APT".into(),
            provenance: "Ubuntu noble/main".into(),
            success: ok,
            message: (!ok).then(|| "E: broken".to_owned()),
        }
    }

    fn temp_file(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("um-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d.join("nested/tracked.db")
    }

    #[test]
    fn fresh_database_is_migrated_to_the_latest_version() {
        let db = Database::open_in_memory().expect("open");
        assert_eq!(db.schema_version().expect("v"), MIGRATIONS.len() as u32);
    }

    #[test]
    fn history_round_trips_newest_first_with_provenance() {
        let db = Database::open_in_memory().expect("open");
        db.add_history(&entry("firefox", true)).expect("add");
        db.add_history(&entry("vlc", false)).expect("add");
        let rows = db.recent_history(10).expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entry, entry("vlc", false));
        assert_eq!(rows[1].entry.provenance, "Ubuntu noble/main");
        assert!(rows[0].timestamp > 0);
        assert_eq!(db.recent_history(1).expect("limit").len(), 1);
    }

    #[test]
    fn reopening_keeps_data_and_does_not_rerun_migrations() {
        let path = temp_file("reopen");
        {
            let db = Database::open(&path).expect("open");
            db.add_history(&entry("firefox", true)).expect("add");
        }
        let db = Database::open(&path).expect("reopen");
        assert_eq!(db.recent_history(10).expect("list").len(), 1);
        assert_eq!(db.schema_version().expect("v"), MIGRATIONS.len() as u32);
        let _ = std::fs::remove_dir_all(path.parent().and_then(|p| p.parent()).expect("dir"));
    }

    #[test]
    fn database_from_a_newer_version_is_refused_not_modified() {
        let path = temp_file("newer");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mk");
        {
            let c = Connection::open(&path).expect("raw");
            c.execute_batch("PRAGMA user_version = 99").expect("set");
        }
        let err = Database::open(&path).err().expect("must fail");
        assert!(matches!(err, DbError::TooNew { found: 99, .. }), "{err}");
        let _ = std::fs::remove_dir_all(path.parent().and_then(|p| p.parent()).expect("dir"));
    }

    fn hydra() -> TrackedRecord {
        TrackedRecord {
            package_name: "hydralauncher".into(),
            github_repo: "hydralauncher/hydra".into(),
            asset_pattern: "hydralauncher_{version}_amd64.deb".into(),
            is_preset: true,
        }
    }

    #[test]
    fn tracked_apps_can_be_added_listed_replaced_and_removed() {
        let db = Database::open_in_memory().expect("open");
        assert!(db.list_tracked().expect("empty").is_empty());
        db.upsert_tracked(&hydra()).expect("add");
        let mut other = hydra();
        other.github_repo = "someone/fork".into();
        other.is_preset = false;
        db.upsert_tracked(&other).expect("replace");
        assert_eq!(db.list_tracked().expect("list"), vec![other]);
        assert!(db.remove_tracked("hydralauncher").expect("remove"));
        assert!(!db.remove_tracked("hydralauncher").expect("again"));
        assert!(db.list_tracked().expect("empty").is_empty());
    }

    #[test]
    fn upgrading_a_version_1_database_keeps_history_and_adds_tracking() {
        let path = temp_file("upgrade");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mk");
        {
            let c = Connection::open(&path).expect("raw");
            c.execute_batch(MIGRATIONS[0]).expect("v1");
            c.execute_batch("PRAGMA user_version = 1").expect("set");
            c.execute(
                "INSERT INTO history (timestamp, app_id, app_name, from_version, to_version, source, provenance, success)
                 VALUES (1, 'apt:x', 'x', '1', '2', 'APT', 'Ubuntu', 1)",
                [],
            )
            .expect("old row");
        }
        let db = Database::open(&path).expect("upgrade");
        assert_eq!(db.schema_version().expect("v"), MIGRATIONS.len() as u32);
        assert_eq!(db.recent_history(5).expect("history").len(), 1);
        db.upsert_tracked(&hydra()).expect("new table works");
        let _ = std::fs::remove_dir_all(path.parent().and_then(|p| p.parent()).expect("dir"));
    }

    #[test]
    fn upgrading_a_version_2_database_keeps_links() {
        let path = temp_file("upgrade2");
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mk");
        {
            let c = Connection::open(&path).expect("raw");
            c.execute_batch(MIGRATIONS[0]).expect("v1");
            c.execute_batch(MIGRATIONS[1]).expect("v2");
            c.execute_batch("PRAGMA user_version = 2").expect("set");
        }
        let db = Database::open(&path).expect("upgrade");
        assert_eq!(db.schema_version().expect("v"), MIGRATIONS.len() as u32);
        db.upsert_tracked(&hydra()).expect("tracked still works");
        let _ = std::fs::remove_dir_all(path.parent().and_then(|p| p.parent()).expect("dir"));
    }

    #[test]
    fn values_are_bound_not_interpolated() {
        let db = Database::open_in_memory().expect("open");
        let mut e = entry("x", true);
        e.app_name = "Robert'); DROP TABLE history;--".into();
        db.add_history(&e).expect("add");
        assert_eq!(
            db.recent_history(5).expect("list")[0].entry.app_name,
            e.app_name
        );
    }
}
