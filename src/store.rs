//! SQLite for recents, settings and a small GitHub metadata cache.
//! Secrets never live here — tokens go to the OS credential vault (see github.rs).

use crate::util;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

const MIGRATIONS: &[&str] = &[
    // v1
    "CREATE TABLE recent_repositories (
        path TEXT PRIMARY KEY,
        name TEXT NOT NULL,
        last_opened_at INTEGER NOT NULL,
        pinned INTEGER NOT NULL DEFAULT 0
     );
     CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE github_cache (
        key TEXT PRIMARY KEY,
        payload TEXT NOT NULL,
        fetched_at INTEGER NOT NULL
     );",
];

pub struct Store {
    conn: Connection,
}

#[derive(Clone, Debug)]
pub struct Recent {
    pub path: String,
    pub name: String,
    pub pinned: bool,
}

/// `GITUST_DATA_DIR` overrides the location (portable installs, tests).
pub fn data_dir() -> PathBuf {
    std::env::var_os("GITUST_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("Gitust"))
}

impl Store {
    pub fn open_default() -> Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(&dir)?;
        Self::open(&dir.join("gitust.db"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", (i + 1) as i64)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    pub fn touch_recent(&self, path: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO recent_repositories (path, name, last_opened_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET name = excluded.name, last_opened_at = excluded.last_opened_at",
            params![path, name, util::now()],
        )?;
        // Keep the list short: drop the oldest unpinned beyond 40.
        self.conn.execute(
            "DELETE FROM recent_repositories WHERE pinned = 0 AND path NOT IN (
                SELECT path FROM recent_repositories ORDER BY pinned DESC, last_opened_at DESC LIMIT 40)",
            [],
        )?;
        Ok(())
    }

    pub fn recents(&self) -> Result<Vec<Recent>> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, name, pinned FROM recent_repositories ORDER BY pinned DESC, last_opened_at DESC")?;
        let rows =
            stmt.query_map([], |r| Ok(Recent { path: r.get(0)?, name: r.get(1)?, pinned: r.get::<_, i64>(2)? != 0 }))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remove_recent(&self, path: &str) -> Result<()> {
        self.conn.execute("DELETE FROM recent_repositories WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn toggle_pin(&self, path: &str) -> Result<()> {
        self.conn.execute("UPDATE recent_repositories SET pinned = 1 - pinned WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM app_settings WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO app_settings (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn cache_get(&self, key: &str) -> Option<(String, i64)> {
        self.conn
            .query_row("SELECT payload, fetched_at FROM github_cache WHERE key = ?1", params![key], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
            .ok()
            .flatten()
    }

    pub fn cache_put(&self, key: &str, payload: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO github_cache (key, payload, fetched_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET payload = excluded.payload, fetched_at = excluded.fetched_at",
            params![key, payload, util::now()],
        )?;
        Ok(())
    }

    pub fn cache_clear(&self) -> Result<()> {
        self.conn.execute("DELETE FROM github_cache", [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recents_and_settings() {
        let s = Store::in_memory().unwrap();
        s.touch_recent("/a", "a").unwrap();
        s.touch_recent("/b", "b").unwrap();
        s.toggle_pin("/a").unwrap();
        let r = s.recents().unwrap();
        assert_eq!(r[0].path, "/a");
        assert!(r[0].pinned);
        s.remove_recent("/b").unwrap();
        assert_eq!(s.recents().unwrap().len(), 1);

        assert_eq!(s.get("theme"), None);
        s.set("theme", "2").unwrap();
        s.set("theme", "1").unwrap();
        assert_eq!(s.get("theme").as_deref(), Some("1"));

        s.cache_put("repos", "[]").unwrap();
        assert_eq!(s.cache_get("repos").unwrap().0, "[]");
        s.cache_clear().unwrap();
        assert!(s.cache_get("repos").is_none());
    }

    #[test]
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        Store::open(&p).unwrap().set("k", "v").unwrap();
        assert_eq!(Store::open(&p).unwrap().get("k").as_deref(), Some("v"));
    }
}
