//! SQLite cache of the signed-in user and their library, so the app can show
//! something instantly on start and work offline.
//!
//! Entries are stored as JSON blobs of [`LibraryEntry`] keyed by `user_book_id`.
//! The database carries a schema version (`PRAGMA user_version`); a file written
//! by a different version is wiped and rebuilt, since everything in it can be
//! re-downloaded.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};

use crate::api::{LibraryEntry, Timestamp, User, UserBookId, UserId};
use crate::{Error, Result};

/// Bump when the table layout or the JSON shape of the stored models changes.
pub const SCHEMA_VERSION: i64 = 1;

const FILE_NAME: &str = "library.sqlite3";

pub struct LibraryCache {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for LibraryCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibraryCache")
            .field("path", &self.path)
            .finish()
    }
}

impl LibraryCache {
    /// `$XDG_DATA_HOME/hardcover-desktop/library.sqlite3`.
    pub fn default_path() -> Result<PathBuf> {
        Ok(crate::project_dirs()?.data_dir().join(FILE_NAME))
    }

    /// Opens (creating if needed) the cache at [`LibraryCache::default_path`].
    pub fn open_default() -> Result<Self> {
        Self::open(Self::default_path()?)
    }

    /// Opens (creating if needed) the cache at `path`, creating parent directories.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Self::init(conn, Some(path.to_owned()))
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            conn.execute_batch(
                "DROP TABLE IF EXISTS library_entries;
                 DROP TABLE IF EXISTS sync_state;
                 DROP TABLE IF EXISTS current_user;",
            )?;
        }
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS current_user (
                 slot INTEGER PRIMARY KEY CHECK (slot = 0),
                 user_id INTEGER NOT NULL,
                 json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS library_entries (
                 user_book_id INTEGER PRIMARY KEY,
                 user_id INTEGER NOT NULL,
                 json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS library_entries_user ON library_entries (user_id);
             CREATE TABLE IF NOT EXISTS sync_state (
                 user_id INTEGER PRIMARY KEY,
                 last_synced_at TEXT NOT NULL
             );",
        )?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The last signed-in user.
    pub fn load_user(&self) -> Result<Option<User>> {
        let json: Option<String> = self
            .conn()
            .query_row("SELECT json FROM current_user WHERE slot = 0", [], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    pub fn save_user(&self, user: &User) -> Result<()> {
        self.conn().execute(
            "INSERT INTO current_user (slot, user_id, json) VALUES (0, ?1, ?2)
             ON CONFLICT (slot) DO UPDATE SET user_id = excluded.user_id, json = excluded.json",
            params![user.id.get(), to_json(user)?],
        )?;
        Ok(())
    }

    /// `user`'s cached entries, ordered by `user_book_id`. Rows that no longer
    /// decode are skipped.
    pub fn load_library(&self, user: UserId) -> Result<Vec<LibraryEntry>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT json FROM library_entries WHERE user_id = ?1 ORDER BY user_book_id")?;
        let rows = stmt.query_map([user.get()], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for json in rows {
            if let Ok(entry) = serde_json::from_str(&json?) {
                out.push(entry);
            }
        }
        Ok(out)
    }

    /// Replaces everything cached for `user` with `entries` (a full sync) and
    /// records the sync time.
    pub fn replace_library(&self, user: UserId, entries: &[LibraryEntry]) -> Result<()> {
        self.replace_library_at(user, entries, chrono::Utc::now())
    }

    fn replace_library_at(
        &self,
        user: UserId,
        entries: &[LibraryEntry],
        now: Timestamp,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM library_entries WHERE user_id = ?1",
            [user.get()],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO library_entries (user_book_id, user_id, json) VALUES (?1, ?2, ?3)",
            )?;
            for e in entries {
                stmt.execute(params![e.user_book_id.get(), user.get(), to_json(e)?])?;
            }
        }
        tx.execute(
            "INSERT INTO sync_state (user_id, last_synced_at) VALUES (?1, ?2)
             ON CONFLICT (user_id) DO UPDATE SET last_synced_at = excluded.last_synced_at",
            params![user.get(), now.to_rfc3339()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Inserts or replaces one entry (e.g. after a mutation), filed under its `user_id`.
    pub fn upsert(&self, entry: &LibraryEntry) -> Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO library_entries (user_book_id, user_id, json) VALUES (?1, ?2, ?3)",
            params![entry.user_book_id.get(), entry.user_id.get(), to_json(entry)?],
        )?;
        Ok(())
    }

    pub fn remove(&self, id: UserBookId) -> Result<()> {
        self.conn().execute(
            "DELETE FROM library_entries WHERE user_book_id = ?1",
            [id.get()],
        )?;
        Ok(())
    }

    /// When [`LibraryCache::replace_library`] last ran for `user`.
    pub fn last_synced_at(&self, user: UserId) -> Result<Option<Timestamp>> {
        let s: Option<String> = self
            .conn()
            .query_row(
                "SELECT last_synced_at FROM sync_state WHERE user_id = ?1",
                [user.get()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(
            s.and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
                .map(|t| t.with_timezone(&chrono::Utc)),
        )
    }

    /// Forgets everything (use on sign-out).
    pub fn clear(&self) -> Result<()> {
        self.conn().execute_batch(
            "DELETE FROM library_entries; DELETE FROM sync_state; DELETE FROM current_user;",
        )?;
        Ok(())
    }
}

fn to_json<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Storage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::*;

    fn entry(id: i64, user: i64, status: ReadingStatus) -> LibraryEntry {
        let mut book = BookSummary::new(BookId::new(100 + id), format!("Synthetic {id}"));
        book.authors = vec!["Jane Quill".into()];
        book.cover = Some(Cover::new("https://assets.hardcover.app/x.jpg"));
        let mut e = LibraryEntry::new(UserBookId::new(id), UserId::new(user), book, status);
        e.rating = Rating::new(3.5);
        let mut read = ReadSession::new(ReadId::new(900 + id));
        read.started_at = Date::from_ymd_opt(2024, 1, 2);
        read.progress_percent = Some(12.5);
        e.current_read = Some(read);
        e.date_added = Date::from_ymd_opt(2023, 12, 31);
        e.updated_at = Some(chrono::Utc::now());
        e
    }

    #[test]
    fn round_trip() {
        let c = LibraryCache::open_in_memory().unwrap();
        let me = UserId::new(1);
        assert!(c.load_library(me).unwrap().is_empty());
        assert!(c.last_synced_at(me).unwrap().is_none());

        let entries = vec![
            entry(2, 1, ReadingStatus::Read),
            entry(1, 1, ReadingStatus::CurrentlyReading),
        ];
        c.replace_library(me, &entries).unwrap();
        let loaded = c.load_library(me).unwrap();
        assert_eq!(loaded, vec![entries[1].clone(), entries[0].clone()]);
        assert!(c.last_synced_at(me).unwrap().is_some());

        let mut changed = entries[0].clone();
        changed.status = ReadingStatus::Paused;
        c.upsert(&changed).unwrap();
        c.upsert(&entry(3, 1, ReadingStatus::WantToRead)).unwrap();
        c.upsert(&entry(4, 2, ReadingStatus::WantToRead)).unwrap(); // another user
        let loaded = c.load_library(me).unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[1].status, ReadingStatus::Paused);

        c.remove(UserBookId::new(3)).unwrap();
        assert_eq!(c.load_library(me).unwrap().len(), 2);

        // A full sync replaces only that user's rows.
        c.replace_library(me, &[entry(5, 1, ReadingStatus::Read)])
            .unwrap();
        assert_eq!(c.load_library(me).unwrap().len(), 1);
        assert_eq!(c.load_library(UserId::new(2)).unwrap().len(), 1);
    }

    #[test]
    fn user_round_trip_and_clear() {
        let c = LibraryCache::open_in_memory().unwrap();
        assert!(c.load_user().unwrap().is_none());
        let mut u = User::new(UserId::new(1), "reader");
        u.name = Some("A. Reader".into());
        c.save_user(&u).unwrap();
        c.save_user(&u).unwrap();
        assert_eq!(c.load_user().unwrap(), Some(u));
        c.replace_library(UserId::new(1), &[entry(1, 1, ReadingStatus::Read)])
            .unwrap();
        c.clear().unwrap();
        assert!(c.load_user().unwrap().is_none());
        assert!(c.load_library(UserId::new(1)).unwrap().is_empty());
        assert!(c.last_synced_at(UserId::new(1)).unwrap().is_none());
    }

    #[test]
    fn persists_on_disk_and_wipes_on_version_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/library.sqlite3");
        {
            let c = LibraryCache::open(&path).unwrap();
            c.replace_library(UserId::new(1), &[entry(1, 1, ReadingStatus::Read)])
                .unwrap();
        }
        {
            let c = LibraryCache::open(&path).unwrap();
            assert_eq!(c.load_library(UserId::new(1)).unwrap().len(), 1);
            c.conn().pragma_update(None, "user_version", 999).unwrap();
        }
        let c = LibraryCache::open(&path).unwrap();
        assert!(c.load_library(UserId::new(1)).unwrap().is_empty());
    }

    #[test]
    fn undecodable_rows_are_skipped() {
        let c = LibraryCache::open_in_memory().unwrap();
        c.upsert(&entry(1, 1, ReadingStatus::Read)).unwrap();
        c.conn()
            .execute(
                "INSERT INTO library_entries VALUES (2, 1, '{\"old\":true}')",
                [],
            )
            .unwrap();
        assert_eq!(c.load_library(UserId::new(1)).unwrap().len(), 1);
    }
}
