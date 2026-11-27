//! The org server's one SQLite store: members, sessions, invites and the
//! audit log. Tokens are kept only as SHA-256.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::path::Path;

use crate::auth::{random_hex, sha256_hex};
use fonsviva_core::util::unix_now;

/// `PRAGMA user_version` of the schema this build writes.
pub const SCHEMA: i64 = 1;

/// An admin link from `init` or `admin-link` lasts a day.
pub const ADMIN_LINK_HOURS: u64 = 24;

pub struct Store {
    db: Connection,
}

const TABLES: &str = "
CREATE TABLE org (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE members (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    role TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    disabled INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE sessions (
    token_sha256 TEXT PRIMARY KEY,
    member_id TEXT NOT NULL REFERENCES members(id),
    created_at INTEGER NOT NULL,
    last_used INTEGER NOT NULL
);
CREATE TABLE invites (
    id TEXT PRIMARY KEY,
    token_sha256 TEXT NOT NULL UNIQUE,
    role TEXT NOT NULL,
    uses INTEGER NOT NULL,
    used INTEGER NOT NULL DEFAULT 0,
    expires_at INTEGER NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0,
    admin_link INTEGER NOT NULL DEFAULT 0,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE audit (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    admin TEXT NOT NULL,
    action TEXT NOT NULL,
    target TEXT NOT NULL
);
";

fn connect(path: &Path) -> Result<Connection> {
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(db)
}

fn audit(db: &Connection, admin: &str, action: &str, target: &str) -> Result<()> {
    db.execute(
        "INSERT INTO audit (at, admin, action, target) VALUES (?1, ?2, ?3, ?4)",
        params![unix_now() as i64, admin, action, target],
    )?;
    Ok(())
}

/// Makes a join link and returns its id and token. The token is shown once.
fn insert_invite(
    db: &Connection,
    role: &str,
    hours: u64,
    uses: u32,
    admin_link: bool,
    by: &str,
) -> Result<(String, String)> {
    let id = format!("inv_{}", random_hex(8));
    let token = random_hex(32);
    let now = unix_now();
    db.execute(
        "INSERT INTO invites (id, token_sha256, role, uses, expires_at, admin_link, created_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            sha256_hex(&token),
            role,
            uses,
            (now + hours * 3600) as i64,
            admin_link,
            by,
            now as i64
        ],
    )?;
    Ok((id, token))
}

impl Store {
    /// Makes the schema in a new, empty `org.db` and records the org's URL.
    pub fn create(path: &Path, url: &str) -> Result<Store> {
        let mut db = connect(path)?;
        let tx = db.transaction()?;
        tx.execute_batch(TABLES)?;
        tx.execute("INSERT INTO org (key, value) VALUES ('url', ?1)", [url])?;
        tx.pragma_update(None, "user_version", SCHEMA)?;
        tx.commit()?;
        Ok(Store { db })
    }

    /// Opens an existing store, refusing one of another schema version.
    pub fn open(path: &Path) -> Result<Store> {
        if !path.is_file() {
            anyhow::bail!(
                "{} does not exist. Run `fonsviva org init` first.",
                path.display()
            );
        }
        let db = connect(path)?;
        let v: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if v != SCHEMA {
            anyhow::bail!(
                "{} has schema version {v}, and this build knows only {SCHEMA}.",
                path.display()
            );
        }
        Ok(Store { db })
    }

    pub fn url(&self) -> Result<String> {
        Ok(self
            .db
            .query_row("SELECT value FROM org WHERE key = 'url'", [], |r| r.get(0))?)
    }

    /// The recovery path: a new admin link, with every unused older one revoked.
    pub fn new_admin_link(&mut self, by: &str) -> Result<String> {
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE invites SET revoked = 1 WHERE admin_link = 1 AND used = 0 AND revoked = 0",
            [],
        )?;
        let (id, token) = insert_invite(&tx, "admin", ADMIN_LINK_HOURS, 1, true, by)?;
        audit(&tx, by, "admin_link.create", &id)?;
        tx.commit()?;
        Ok(token)
    }
}
