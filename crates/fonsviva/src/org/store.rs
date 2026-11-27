//! The org server's one SQLite store: members, sessions, invites, the
//! audit log, model sources and settings. Tokens are kept only as SHA-256,
//! and source keys only sealed under `secrets.key` (`seal`).

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

use super::seal;

use crate::auth::{ct_eq, random_hex, sha256_hex};
use fonsviva_core::util::{rfc3339, unix_now};

/// `PRAGMA user_version` of the schema this build writes.
pub const SCHEMA: i64 = 2;

pub const DAY: u64 = 24 * 60 * 60;
/// A session ends this long after its last use.
pub const SESSION_IDLE: u64 = 30 * DAY;
/// An admin link from `init` or `admin-link` lasts a day.
pub const ADMIN_LINK_HOURS: u64 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Admin,
    Full,
    User,
    Student,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Full => "full",
            Role::User => "user",
            Role::Student => "student",
        }
    }

    pub fn parse(s: &str) -> Option<Role> {
        Some(match s {
            "admin" => Role::Admin,
            "full" => Role::Full,
            "user" => Role::User,
            "student" => Role::Student,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub role: Role,
}

/// A member as admins see them.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemberRow {
    pub id: String,
    pub name: String,
    pub role: &'static str,
    pub created: String,
    pub disabled: bool,
}

/// An invite as admins see it: never its token.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InviteRow {
    pub id: String,
    pub role: String,
    pub uses: i64,
    pub used: i64,
    pub expires_at: String,
    pub revoked: bool,
    pub admin_link: bool,
    pub created_by: String,
    pub created: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditRow {
    pub at: String,
    pub admin: String,
    pub action: String,
    pub target: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MemberRefused {
    NotFound,
    LastAdmin,
}

/// Why a join link didn't work. The caller answers every one the same way.
#[derive(Debug, PartialEq, Eq)]
pub enum JoinRefused {
    Gone,
}

/// A source's key, opened from its sealed copy.
#[derive(Clone)]
pub enum SourceKey {
    None,
    Key(String),
    /// `secrets.key` is missing or isn't the key it was sealed under.
    Unreadable,
}

impl std::fmt::Debug for SourceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SourceKey::None => "None",
            SourceKey::Key(_) => "Key(<redacted>)",
            SourceKey::Unreadable => "Unreadable",
        })
    }
}

/// Why a source was not added.
#[derive(Debug, PartialEq, Eq)]
pub enum AddRefused {
    NameTaken,
    /// Keys are sealed but `secrets.key` is gone or not a key: a new one
    /// would hide them.
    SecretsKeyMissing,
    /// No key is sealed yet, but `secrets.key` is there and not a key.
    SecretsKeyBad,
}

/// A model source. Its key never leaves the server except in that
/// source's own `Authorization` header.
#[derive(Debug, Clone)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: SourceKey,
    pub local: bool,
    pub created: i64,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub students_allowed: bool,
}

pub struct Store {
    db: Connection,
    /// The folder holding org.db and `secrets.key`.
    dir: PathBuf,
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

/// Each entry moves the schema from its index plus one to the next version.
const MIGRATIONS: &[&str] = &["
CREATE TABLE sources (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    base_url TEXT NOT NULL,
    api_key BLOB,
    local INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
"];

fn migrate(db: &mut Connection, from: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let now: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if now == from {
        for step in &MIGRATIONS[(from - 1) as usize..] {
            tx.execute_batch(step)?;
        }
        tx.pragma_update(None, "user_version", SCHEMA)?;
    }
    tx.commit()?;
    Ok(())
}

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

fn dir_of(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

fn time(t: i64) -> String {
    rfc3339(t.max(0) as u64)
}

impl Store {
    /// Makes the schema in a new, empty `org.db` and records the org's URL.
    pub fn create(path: &Path, url: &str) -> Result<Store> {
        let mut db = connect(path)?;
        let tx = db.transaction()?;
        tx.execute_batch(TABLES)?;
        tx.execute("INSERT INTO org (key, value) VALUES ('url', ?1)", [url])?;
        tx.pragma_update(None, "user_version", 1)?;
        tx.commit()?;
        migrate(&mut db, 1)?;
        Ok(Store {
            db,
            dir: dir_of(path),
        })
    }

    /// Opens an existing store, refusing one of another schema version.
    pub fn open(path: &Path) -> Result<Store> {
        if !path.is_file() {
            anyhow::bail!(
                "{} does not exist. Run `fonsviva org init` first.",
                path.display()
            );
        }
        let mut db = connect(path)?;
        let v: i64 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if !(1..=SCHEMA).contains(&v) {
            anyhow::bail!(
                "{} has schema version {v}, and this build knows 1 to {SCHEMA}.",
                path.display()
            );
        }
        if v < SCHEMA {
            migrate(&mut db, v)?;
        }
        Ok(Store {
            db,
            dir: dir_of(path),
        })
    }

    pub fn url(&self) -> Result<String> {
        Ok(self
            .db
            .query_row("SELECT value FROM org WHERE key = 'url'", [], |r| r.get(0))?)
    }

    /// Makes a join link for `role` and records it; the token is returned once.
    pub fn create_invite(
        &mut self,
        role: Role,
        hours: u64,
        uses: u32,
        by: &str,
    ) -> Result<(String, String)> {
        let tx = self.db.transaction()?;
        let (id, token) = insert_invite(&tx, role.as_str(), hours, uses, false, by)?;
        audit(&tx, by, "invite.create", &id)?;
        tx.commit()?;
        Ok((id, token))
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

    /// Uses a join link: makes the member with the link's role and a session.
    /// Returns the member and the session token.
    pub fn join(
        &mut self,
        token: &str,
        name: &str,
    ) -> Result<std::result::Result<(Member, String), JoinRefused>> {
        let hash = sha256_hex(token);
        let now = unix_now() as i64;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(String, String, String, i64, i64, i64, bool)> = tx
            .query_row(
                "SELECT id, token_sha256, role, uses, used, expires_at, revoked FROM invites WHERE token_sha256 = ?1",
                [&hash],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, stored, role, uses, used, expires_at, revoked)) = row else {
            return Ok(Err(JoinRefused::Gone));
        };
        if !ct_eq(&stored, &hash) || revoked || used >= uses || expires_at <= now {
            return Ok(Err(JoinRefused::Gone));
        }
        let role = Role::parse(&role).context("invite role")?;
        tx.execute("UPDATE invites SET used = used + 1 WHERE id = ?1", [&id])?;
        let member = Member {
            id: format!("m_{}", random_hex(8)),
            name: name.to_string(),
            role,
        };
        tx.execute(
            "INSERT INTO members (id, name, role, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![member.id, member.name, role.as_str(), now],
        )?;
        let session = random_hex(32);
        tx.execute(
            "INSERT INTO sessions (token_sha256, member_id, created_at, last_used) VALUES (?1, ?2, ?3, ?3)",
            params![sha256_hex(&session), member.id, now],
        )?;
        tx.commit()?;
        Ok(Ok((member, session)))
    }

    /// The member a session token belongs to, read fresh so a role change
    /// counts on the next request. Marks the session used.
    pub fn session(&self, token: &str) -> Result<Option<Member>> {
        let hash = sha256_hex(token);
        let now = unix_now() as i64;
        let row: Option<(String, String, String, String, i64, bool)> = self
            .db
            .query_row(
                "SELECT s.token_sha256, m.id, m.name, m.role, s.last_used, m.disabled
                 FROM sessions s JOIN members m ON m.id = s.member_id WHERE s.token_sha256 = ?1",
                [&hash],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored, id, name, role, last_used, disabled)) = row else {
            return Ok(None);
        };
        if !ct_eq(&stored, &hash) {
            return Ok(None);
        }
        if disabled || last_used + SESSION_IDLE as i64 <= now {
            self.db
                .execute("DELETE FROM sessions WHERE token_sha256 = ?1", [&hash])?;
            return Ok(None);
        }
        self.db.execute(
            "UPDATE sessions SET last_used = ?1 WHERE token_sha256 = ?2",
            params![now, hash],
        )?;
        let role = Role::parse(&role).context("member role")?;
        Ok(Some(Member { id, name, role }))
    }

    pub fn end_session(&self, token: &str) -> Result<()> {
        self.db.execute(
            "DELETE FROM sessions WHERE token_sha256 = ?1",
            [sha256_hex(token)],
        )?;
        Ok(())
    }

    pub fn members(&self) -> Result<Vec<MemberRow>> {
        let mut q = self
            .db
            .prepare("SELECT id, name, role, created_at, disabled FROM members ORDER BY rowid")?;
        let rows = q.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?;
        let mut out = vec![];
        for row in rows {
            let (id, name, role, created, disabled) = row?;
            out.push(MemberRow {
                id,
                name,
                role: Role::parse(&role).context("member role")?.as_str(),
                created: time(created),
                disabled,
            });
        }
        Ok(out)
    }

    /// Changes a member's role or disabled flag. An admin can be demoted or
    /// disabled only while another enabled admin has a live session, because
    /// a member with no session can't sign in again. Disabling ends every
    /// session the member has. Disabling or demoting revokes the live links
    /// the member made. Only real changes are written.
    pub fn update_member(
        &mut self,
        by: &str,
        id: &str,
        role: Option<Role>,
        disabled: Option<bool>,
    ) -> Result<std::result::Result<(), MemberRefused>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let row: Option<(String, bool)> = tx
            .query_row(
                "SELECT role, disabled FROM members WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((was_role, was_disabled)) = row else {
            return Ok(Err(MemberRefused::NotFound));
        };
        let was_admin = was_role == Role::Admin.as_str() && !was_disabled;
        let stays_admin = role.map_or(was_role == Role::Admin.as_str(), |r| r == Role::Admin)
            && !disabled.unwrap_or(was_disabled);
        let now = unix_now() as i64;
        if was_admin && !stays_admin {
            let others: i64 = tx.query_row(
                "SELECT COUNT(DISTINCT m.id) FROM members m JOIN sessions s ON s.member_id = m.id
                 WHERE m.role = 'admin' AND m.disabled = 0 AND m.id != ?1 AND s.last_used + ?2 > ?3",
                params![id, SESSION_IDLE as i64, now],
                |r| r.get(0),
            )?;
            if others == 0 {
                return Ok(Err(MemberRefused::LastAdmin));
            }
        }
        let role = role.filter(|r| r.as_str() != was_role);
        let disabled = disabled.filter(|d| *d != was_disabled);
        if let Some(r) = role {
            tx.execute(
                "UPDATE members SET role = ?1 WHERE id = ?2",
                params![r.as_str(), id],
            )?;
            audit(&tx, by, &format!("member.role={}", r.as_str()), id)?;
        }
        if let Some(d) = disabled {
            tx.execute(
                "UPDATE members SET disabled = ?1 WHERE id = ?2",
                params![d, id],
            )?;
            if d {
                tx.execute("DELETE FROM sessions WHERE member_id = ?1", [id])?;
            }
            let action = if d { "member.disable" } else { "member.enable" };
            audit(&tx, by, action, id)?;
        }
        if disabled == Some(true) || (was_admin && !stays_admin) {
            let mut q = tx.prepare(
                "SELECT id FROM invites
                 WHERE created_by = ?1 AND revoked = 0 AND used < uses AND expires_at > ?2",
            )?;
            let open: Vec<String> = q
                .query_map(params![id, now], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(q);
            for invite in open {
                tx.execute("UPDATE invites SET revoked = 1 WHERE id = ?1", [&invite])?;
                audit(&tx, by, "invite.revoke", &invite)?;
            }
        }
        tx.commit()?;
        Ok(Ok(()))
    }

    pub fn invites(&self) -> Result<Vec<InviteRow>> {
        let mut q = self.db.prepare(
            "SELECT id, role, uses, used, expires_at, revoked, admin_link, created_by, created_at
             FROM invites ORDER BY rowid DESC",
        )?;
        let rows = q.query_map([], |r| {
            Ok(InviteRow {
                id: r.get(0)?,
                role: r.get(1)?,
                uses: r.get(2)?,
                used: r.get(3)?,
                expires_at: time(r.get(4)?),
                revoked: r.get(5)?,
                admin_link: r.get(6)?,
                created_by: r.get(7)?,
                created: time(r.get(8)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revokes a link. Revoking one already revoked changes nothing and
    /// writes no audit row.
    pub fn revoke_invite(&mut self, by: &str, id: &str) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM invites WHERE id = ?1)",
            [id],
            |r| r.get(0),
        )?;
        let n = tx.execute(
            "UPDATE invites SET revoked = 1 WHERE id = ?1 AND revoked = 0",
            [id],
        )?;
        if n == 1 {
            audit(&tx, by, "invite.revoke", id)?;
        }
        tx.commit()?;
        Ok(exists)
    }

    /// Every admin action, newest first.
    pub fn audit_log(&self) -> Result<Vec<AuditRow>> {
        let mut q = self
            .db
            .prepare("SELECT at, admin, action, target FROM audit ORDER BY seq DESC")?;
        let rows = q.query_map([], |r| {
            Ok(AuditRow {
                at: time(r.get(0)?),
                admin: r.get(1)?,
                action: r.get(2)?,
                target: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Adds a source and its audit row. A keyed source is refused while
    /// sealed keys are stored and `secrets.key` is missing.
    pub fn add_source(
        &mut self,
        by: &str,
        name: &str,
        base_url: &str,
        api_key: Option<&str>,
        local: bool,
    ) -> Result<Result<String, AddRefused>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let taken: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sources WHERE name = ?1)",
            [name],
            |r| r.get(0),
        )?;
        if taken {
            return Ok(Err(AddRefused::NameTaken));
        }
        if api_key.is_some() && seal::read(&self.dir).is_none() {
            let sealed: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM sources WHERE api_key IS NOT NULL)",
                [],
                |r| r.get(0),
            )?;
            if sealed {
                return Ok(Err(AddRefused::SecretsKeyMissing));
            }
            if seal::exists(&self.dir) {
                return Ok(Err(AddRefused::SecretsKeyBad));
            }
        }
        let id = format!("src_{}", random_hex(8));
        let sealed = match api_key {
            Some(k) => Some(seal::seal(&seal::ensure(&self.dir)?, &id, k)),
            None => None,
        };
        tx.execute(
            "INSERT INTO sources (id, name, base_url, api_key, local, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, name, base_url, sealed, local, unix_now() as i64],
        )?;
        audit(&tx, by, "source.add", name)?;
        tx.commit()?;
        Ok(Ok(id))
    }

    /// Every source, its key opened with `secrets.key` as it is now.
    pub fn sources(&self) -> Result<Vec<Source>> {
        let secret = seal::read(&self.dir);
        let mut q = self.db.prepare(
            "SELECT id, name, base_url, api_key, local, created_at FROM sources ORDER BY rowid",
        )?;
        let rows = q.query_map([], |r| {
            let id: String = r.get(0)?;
            let sealed: Option<Vec<u8>> = r.get(3)?;
            let api_key = match sealed {
                None => SourceKey::None,
                Some(b) => match secret.and_then(|k| seal::open(&k, &id, &b)) {
                    Some(k) => SourceKey::Key(k),
                    None => SourceKey::Unreadable,
                },
            };
            Ok(Source {
                id,
                name: r.get(1)?,
                base_url: r.get(2)?,
                api_key,
                local: r.get(4)?,
                created: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Removes a source and writes its audit row; false when there is none.
    pub fn remove_source(&mut self, by: &str, id: &str) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let name: Option<String> = tx
            .query_row("SELECT name FROM sources WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        let Some(name) = name else {
            return Ok(false);
        };
        tx.execute("DELETE FROM sources WHERE id = ?1", [id])?;
        audit(&tx, by, "source.remove", &name)?;
        tx.commit()?;
        Ok(true)
    }

    pub fn settings(&self) -> Result<Settings> {
        let students: Option<String> = self
            .db
            .query_row(
                "SELECT value FROM org WHERE key = 'students_allowed'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(Settings {
            students_allowed: students.as_deref() == Some("true"),
        })
    }

    /// Writes the settings that change, with one audit row naming them.
    pub fn update_settings(
        &mut self,
        by: &str,
        students_allowed: Option<bool>,
    ) -> Result<Settings> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let was: Option<String> = tx
            .query_row(
                "SELECT value FROM org WHERE key = 'students_allowed'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let was = was.as_deref() == Some("true");
        let mut changed = vec![];
        if let Some(s) = students_allowed.filter(|s| *s != was) {
            tx.execute(
                "INSERT INTO org (key, value) VALUES ('students_allowed', ?1)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                [if s { "true" } else { "false" }],
            )?;
            changed.push("studentsAllowed");
        }
        if !changed.is_empty() {
            audit(&tx, by, "settings.update", &changed.join(","))?;
        }
        tx.commit()?;
        self.settings()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyed_source_prints_no_key() {
        let s = Source {
            id: "src_a".into(),
            name: "a".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: SourceKey::Key("sk-debug-secret-91c2".into()),
            local: false,
            created: 0,
        };
        for shown in [
            format!("{s:?}"),
            format!("{s:#?}"),
            format!("{:?}", s.api_key),
        ] {
            assert!(!shown.contains("sk-debug-secret-91c2"), "{shown}");
            assert!(shown.contains("Key(<redacted>)"), "{shown}");
        }
    }
}
