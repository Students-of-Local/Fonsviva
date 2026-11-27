//! The org server's one SQLite store: members, sessions, invites and the
//! audit log. Tokens are kept only as SHA-256.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

use crate::auth::{ct_eq, random_hex, sha256_hex};
use fonsviva_core::util::{rfc3339, unix_now};

/// `PRAGMA user_version` of the schema this build writes.
pub const SCHEMA: i64 = 1;

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
        let tx = self.db.transaction()?;
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

    /// Changes a member's role or disabled flag. The last enabled admin can't
    /// be demoted or disabled. Disabling ends every session the member has.
    pub fn update_member(
        &mut self,
        by: &str,
        id: &str,
        role: Option<Role>,
        disabled: Option<bool>,
    ) -> Result<std::result::Result<(), MemberRefused>> {
        let tx = self.db.transaction()?;
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
        if was_admin && !stays_admin {
            let admins: i64 = tx.query_row(
                "SELECT COUNT(*) FROM members WHERE role = 'admin' AND disabled = 0",
                [],
                |r| r.get(0),
            )?;
            if admins <= 1 {
                return Ok(Err(MemberRefused::LastAdmin));
            }
        }
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

    pub fn revoke_invite(&self, by: &str, id: &str) -> Result<bool> {
        let n = self
            .db
            .execute("UPDATE invites SET revoked = 1 WHERE id = ?1", [id])?;
        if n == 1 {
            audit(&self.db, by, "invite.revoke", id)?;
        }
        Ok(n == 1)
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
}
