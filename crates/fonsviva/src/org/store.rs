//! The org server's one SQLite store: members, sessions, invites, the
//! audit log, model sources, settings, personal keys, usage, each
//! member's limits, thread and compute requests. Tokens and personal keys
//! are kept only as SHA-256, and source keys only sealed under
//! `secrets.key` (`seal`). Message text lives only in `messages` and its
//! FTS index, and compute prompts and answers only in `compute_prompts`
//! and `compute_results`.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

use super::chat;
use super::costs::{self, Pricing};
use super::limits::Limits;
use super::seal;
use super::web;

use crate::auth::{ct_eq, random_hex, sha256_hex};
use fonsviva_core::util::{rfc3339, unix_now};

/// The most messages a send's context looks back over, kept or skipped,
/// so a long run of one role never walks a whole thread under the lock.
pub const CONTEXT_SCAN: i64 = 1000;

/// `PRAGMA user_version` of the schema this build writes.
pub const SCHEMA: i64 = 10;

pub const DAY: u64 = 24 * 60 * 60;
/// A session ends this long after its last use.
pub const SESSION_IDLE: u64 = 30 * DAY;
/// An admin link from `init` or `admin-link` lasts a day.
pub const ADMIN_LINK_HOURS: u64 = 24;
/// Live personal keys one member may hold.
pub const MAX_KEYS: i64 = 20;
/// The most text one message may hold, in bytes.
pub const TEXT_LIMIT: usize = 256 << 10;
/// Private use characters around search matches.
pub const MARK_OPEN: char = '\u{E000}';
pub const MARK_CLOSE: char = '\u{E001}';
/// Bounded merges one cleanup runs at most.
pub const MERGES: usize = 1000;

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
    pub pricing: Option<Pricing>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub students_allowed: bool,
    pub daily_tokens: Option<u64>,
    pub keep_threads: bool,
    pub admins_read_threads: bool,
    /// Micro-USD, sent as a decimal string.
    #[serde(serialize_with = "as_decimal")]
    pub electricity_per_kwh: i64,
    /// The IANA zone members' hours are read in.
    pub timezone: String,
    /// The org's SearXNG, while search is on, and whether it is on the
    /// org's own network.
    pub search_url: Option<String>,
    #[serde(skip)]
    pub search_local: bool,
    /// What leaves the org when search is on and off its network.
    pub search_notice: Option<String>,
}

fn as_string<S: serde::Serializer>(n: &i64, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.collect_str(n)
}

fn as_decimal<S: serde::Serializer>(m: &i64, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&costs::decimal(*m))
}

/// The settings a `PATCH /settings` changes; `None` leaves one as it is.
/// `daily_tokens` is `Some(None)` to remove the limit.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SettingsPatch {
    pub students_allowed: Option<bool>,
    pub daily_tokens: Option<Option<u64>>,
    pub keep_threads: Option<bool>,
    pub admins_read_threads: Option<bool>,
    pub electricity_per_kwh: Option<i64>,
    pub timezone: Option<String>,
    /// `Some(None)` turns search off; a URL comes with whether it is local.
    pub search_url: Option<Option<(String, bool)>>,
}

/// One message of a member's thread.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MessageRow {
    pub seq: i64,
    pub role: String,
    pub text: String,
    pub model: String,
    pub at: String,
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepRow>,
}

/// A tool call of a stored reply as its thread shows it: never its result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StepRow {
    pub name: String,
    pub args: serde_json::Value,
    pub ok: bool,
}

/// A tool call and its result, as a send keeps them under its reply.
pub struct Step {
    pub name: String,
    pub args: String,
    pub ok: bool,
    pub result: String,
}

/// Artifacts a member keeps at most, and versions an artifact has.
pub const MAX_ARTIFACTS: i64 = 500;
pub const MAX_VERSIONS: i64 = 20;

/// An artifact as its owner lists it: never its content.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRow {
    pub id: String,
    #[serde(skip)]
    pub member: String,
    pub title: String,
    pub kind: String,
    pub language: Option<String>,
    pub version: i64,
    pub message_seq: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionRow {
    pub version: i64,
    pub bytes: i64,
    pub created_at: String,
}

/// Why an artifact was not made or changed.
#[derive(Debug, PartialEq, Eq)]
pub enum ArtifactRefused {
    TooMany,
    NotFound,
    TooManyVersions,
}

/// A search hit: each match in `snippet` is set between `MARK_OPEN` and
/// `MARK_CLOSE`, which stored text never holds.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchRow {
    pub seq: i64,
    pub role: String,
    pub at: String,
    pub snippet: String,
}

/// Why a message was not stored.
#[derive(Debug, PartialEq, Eq)]
pub enum TextRefused {
    Empty,
    TooLarge,
}

/// A personal key as its member sees it: never the key.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyRow {
    pub id: String,
    pub name: String,
    pub created: String,
    pub last_used: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub day: String,
    pub member: String,
    pub model: String,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Some row in the group had no counts from its source.
    pub estimated: bool,
    /// Nano-USD, sent as a decimal string so no client rounds it.
    #[serde(serialize_with = "as_string")]
    pub cost_nanos: i64,
    /// Every row in the group had pricing.
    pub cost_known: bool,
}

/// Usage over a range for one member or model, or for all of it.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TotalRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub requests: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    /// Nano-USD, sent as a decimal string so no client rounds it.
    #[serde(serialize_with = "as_string")]
    pub cost_nanos: i64,
    pub cost_known: bool,
    pub estimated: bool,
}

/// What `totals` groups by.
#[derive(Debug, Clone, Copy)]
pub enum By {
    Member,
    Model,
}

/// One proxied request. Never any message text.
pub struct Usage<'a> {
    pub member: &'a str,
    pub key: Option<&'a str>,
    pub model: &'a str,
    pub status: u16,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// The counts are estimates: the source reported none.
    pub estimated: bool,
    pub ms: i64,
    /// The compute request the row is a prompt of.
    pub compute: Option<&'a str>,
}

/// Statuses a compute request is still open in.
pub const COMPUTE_OPEN: [&str; 3] = ["pending", "approved", "running"];
/// Open compute requests one member may hold.
pub const MAX_OPEN_COMPUTE: i64 = 10;
/// Compute requests one member may keep, open or finished, until they
/// delete some.
pub const MAX_KEPT_COMPUTE: i64 = 50;

/// A compute request as its member files it.
pub struct NewCompute<'a> {
    pub member: &'a str,
    pub model: &'a str,
    pub prompts: &'a [String],
    pub start: i64,
    pub end: i64,
    pub budget: i64,
    pub note: Option<&'a str>,
}

/// The window and budget an admin approved, and who and when.
#[derive(Debug, Clone)]
pub struct Approved {
    pub start: i64,
    pub end: i64,
    pub budget: i64,
    pub by: String,
    pub at: i64,
}

/// A compute request with its counts and usage. Never a prompt.
#[derive(Debug, Clone)]
pub struct ComputeRow {
    pub id: String,
    pub member: String,
    pub name: String,
    pub model: String,
    /// Start, end and `maxTokens` as the member asked.
    pub asked: (i64, i64, i64),
    pub approved: Option<Approved>,
    pub status: String,
    pub reason: Option<String>,
    pub note: Option<String>,
    pub prompts: i64,
    pub done: i64,
    pub usage: TotalRow,
    pub created: i64,
    pub started: Option<i64>,
    pub ended: Option<i64>,
}

/// An approved or running compute request as the scheduler reads it.
pub struct Due {
    pub id: String,
    pub member: String,
    pub running: bool,
    pub start: i64,
    pub end: i64,
}

/// A prompt's answer as the runner keeps it.
pub struct Answer<'a> {
    pub text: &'a str,
    pub status: u16,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub cost_nanos: i64,
}

/// One answered prompt of a compute request.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputeResult {
    pub index: i64,
    pub prompt: String,
    pub text: String,
    pub status: u16,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    #[serde(serialize_with = "as_string")]
    pub cost_nanos: i64,
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
const MIGRATIONS: &[&str] = &[
    "
CREATE TABLE sources (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    base_url TEXT NOT NULL,
    api_key BLOB,
    local INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
",
    "
CREATE TABLE usage (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    at INTEGER NOT NULL,
    member_id TEXT NOT NULL,
    key_id TEXT,
    model TEXT NOT NULL,
    status INTEGER NOT NULL,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    ms INTEGER NOT NULL
);
CREATE INDEX usage_member_at ON usage (member_id, at);
",
    "
CREATE TABLE keys (
    id TEXT PRIMARY KEY,
    key_sha256 TEXT NOT NULL UNIQUE,
    member_id TEXT NOT NULL REFERENCES members(id),
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used INTEGER
);
",
    "
ALTER TABLE usage ADD COLUMN estimated INTEGER NOT NULL DEFAULT 0;
",
    "
CREATE TABLE messages (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    member_id TEXT NOT NULL REFERENCES members(id),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    text TEXT NOT NULL,
    model TEXT NOT NULL,
    at INTEGER NOT NULL,
    status INTEGER
);
CREATE INDEX messages_member_seq ON messages (member_id, seq);
CREATE VIRTUAL TABLE messages_fts USING fts5 (text, content = 'messages', content_rowid = 'seq');
CREATE TRIGGER messages_fts_insert AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts (rowid, text) VALUES (new.seq, new.text);
END;
CREATE TRIGGER messages_fts_delete AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts (messages_fts, rowid, text) VALUES ('delete', old.seq, old.text);
END;
INSERT INTO messages_fts (messages_fts, rank) VALUES ('automerge', 4);
",
    "
ALTER TABLE sources ADD COLUMN pricing TEXT;
ALTER TABLE usage ADD COLUMN cost_nanos INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage ADD COLUMN cost_known INTEGER NOT NULL DEFAULT 0;
",
    "
CREATE TABLE limits (
    member_id TEXT PRIMARY KEY REFERENCES members(id),
    models TEXT,
    daily_tokens INTEGER,
    hours TEXT
);
",
    "
CREATE TABLE compute (
    id TEXT PRIMARY KEY,
    member_id TEXT NOT NULL REFERENCES members(id),
    model TEXT NOT NULL,
    start_asked INTEGER NOT NULL,
    end_asked INTEGER NOT NULL,
    max_asked INTEGER NOT NULL,
    start INTEGER,
    end INTEGER,
    max_tokens INTEGER,
    status TEXT NOT NULL,
    reason TEXT,
    note TEXT,
    approved_by TEXT,
    approved_at INTEGER,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    ended_at INTEGER
);
CREATE INDEX compute_member ON compute (member_id, status);
CREATE TABLE compute_prompts (
    compute_id TEXT NOT NULL REFERENCES compute(id),
    idx INTEGER NOT NULL,
    prompt TEXT NOT NULL,
    PRIMARY KEY (compute_id, idx)
);
CREATE TABLE compute_results (
    compute_id TEXT NOT NULL REFERENCES compute(id),
    idx INTEGER NOT NULL,
    text TEXT NOT NULL,
    status INTEGER NOT NULL,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    cost_nanos INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (compute_id, idx)
);
ALTER TABLE usage ADD COLUMN compute_id TEXT;
CREATE INDEX usage_compute ON usage (compute_id);
",
    "
CREATE TABLE message_steps (
    reply_seq INTEGER NOT NULL REFERENCES messages(seq) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    name TEXT NOT NULL,
    args TEXT NOT NULL,
    ok INTEGER NOT NULL,
    result TEXT NOT NULL,
    PRIMARY KEY (reply_seq, idx)
);
CREATE TABLE artifacts (
    id TEXT PRIMARY KEY,
    member_id TEXT NOT NULL REFERENCES members(id),
    reply_seq INTEGER REFERENCES messages(seq) ON DELETE SET NULL,
    title TEXT NOT NULL,
    kind TEXT NOT NULL,
    language TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX artifacts_member ON artifacts (member_id, created_at);
CREATE INDEX artifacts_reply ON artifacts (reply_seq);
CREATE TABLE artifact_versions (
    artifact_id TEXT NOT NULL REFERENCES artifacts(id),
    version INTEGER NOT NULL,
    content TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (artifact_id, version)
);
",
];

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
    db.pragma_update(None, "secure_delete", true)?;
    Ok(db)
}

/// Inserts a message with U+E000 and U+E001 stripped and returns its seq,
/// refused when it is over `TEXT_LIMIT` or, unless `blank` allows it, only
/// whitespace.
fn insert_message(
    db: &Connection,
    member: &str,
    (role, text, model, status): (&str, &str, &str, Option<u16>),
    blank: bool,
) -> Result<Result<i64, TextRefused>> {
    let text = text.replace([MARK_OPEN, MARK_CLOSE], "");
    if !blank && text.trim().is_empty() {
        return Ok(Err(TextRefused::Empty));
    }
    if text.len() > TEXT_LIMIT {
        return Ok(Err(TextRefused::TooLarge));
    }
    db.execute(
        "INSERT INTO messages (member_id, role, text, model, at, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![member, role, text, model, unix_now() as i64, status],
    )?;
    Ok(Ok(db.last_insert_rowid()))
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

fn setting(db: &Connection, key: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("SELECT value FROM org WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

fn set(db: &Connection, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(v) => db.execute(
            "INSERT INTO org (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, v],
        )?,
        None => db.execute("DELETE FROM org WHERE key = ?1", [key])?,
    };
    Ok(())
}

fn read_settings(db: &Connection) -> Result<Settings> {
    let search_url = setting(db, "search_url")?;
    let search_local = setting(db, "search_local")?.as_deref() == Some("true");
    let daily = setting(db, "daily_tokens")?
        .map(|v| v.parse::<u64>())
        .transpose()
        .context("daily_tokens setting")?;
    Ok(Settings {
        students_allowed: setting(db, "students_allowed")?.as_deref() == Some("true"),
        daily_tokens: daily,
        keep_threads: setting(db, "keep_threads")?.as_deref() != Some("false"),
        admins_read_threads: setting(db, "admins_read_threads")?.as_deref() == Some("true"),
        electricity_per_kwh: setting(db, "electricity_per_kwh")?
            .map(|v| v.parse::<i64>())
            .transpose()
            .context("electricity_per_kwh setting")?
            .unwrap_or(costs::KWH_DEFAULT),
        timezone: setting(db, "timezone")?.unwrap_or_else(|| "UTC".into()),
        search_notice: search_url
            .as_deref()
            .filter(|_| !search_local)
            .and_then(|u| url::Url::parse(u).ok())
            .map(|u| web::notice(&u)),
        search_url,
        search_local,
    })
}

fn pricing(stored: Option<String>) -> Result<Option<Pricing>> {
    let Some(s) = stored else {
        return Ok(None);
    };
    let v: serde_json::Value = serde_json::from_str(&s).context("source pricing")?;
    Pricing::parse(&v).context("source pricing")
}

/// Joins a cost summed in two parts, whole 10^9 nanos and the rest, so no
/// group of saturated rows overflows SQLite's integer `SUM`.
fn nanos(whole: i64, rest: i64) -> i64 {
    let n = i128::from(whole) * 1_000_000_000 + i128::from(rest);
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A `limits` row joined to its member: role, models, daily tokens, hours.
type LimitsRow = (String, Option<String>, Option<i64>, Option<String>);

/// The member's role and their limits row as it parses.
fn limits_row(db: &Connection, member: &str) -> Result<Option<(Role, Result<Limits>)>> {
    let row: Option<LimitsRow> = db
        .query_row(
            "SELECT m.role, l.models, l.daily_tokens, l.hours
             FROM members m LEFT JOIN limits l ON l.member_id = m.id WHERE m.id = ?1",
            [member],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((role, models, daily, hours)) = row else {
        return Ok(None);
    };
    let role = Role::parse(&role).context("member role")?;
    Ok(Some((role, Limits::from_columns(models, daily, hours))))
}

fn read_limits(db: &Connection, member: &str) -> Result<Option<(Role, Limits)>> {
    let Some((role, l)) = limits_row(db, member)? else {
        return Ok(None);
    };
    Ok(Some((role, l?)))
}

/// The member's role and stored limits, a row that no longer parses read
/// as none so that a new one can replace it.
fn read_limits_or_default(db: &Connection, member: &str) -> Result<Option<(Role, Limits)>> {
    let Some((role, l)) = limits_row(db, member)? else {
        return Ok(None);
    };
    let was = l.unwrap_or_else(|e| {
        eprintln!("fonsviva org: {e:#}");
        Limits::default()
    });
    Ok(Some((role, was)))
}

fn flag(b: bool) -> Option<&'static str> {
    Some(if b { "true" } else { "false" })
}

/// The words of a search, each a quoted FTS5 phrase, so that `"`, `*`,
/// `NEAR(` or `text:` in it are only text.
fn phrases(q: &str) -> String {
    q.split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

const COMPUTE_SELECT: &str = "
SELECT c.id, c.member_id, m.name, c.model, c.start_asked, c.end_asked, c.max_asked,
       c.start, c.end, c.max_tokens, c.approved_by, c.approved_at, c.status, c.reason,
       c.note, c.created_at, c.started_at, c.ended_at,
       (SELECT COUNT(*) FROM compute_prompts p WHERE p.compute_id = c.id),
       (SELECT COUNT(*) FROM compute_results r WHERE r.compute_id = c.id),
       (SELECT COUNT(*) FROM usage u WHERE u.compute_id = c.id),
       COALESCE((SELECT SUM(prompt_tokens) FROM usage u WHERE u.compute_id = c.id), 0),
       COALESCE((SELECT SUM(completion_tokens) FROM usage u WHERE u.compute_id = c.id), 0),
       COALESCE((SELECT SUM(cost_nanos / 1000000000) FROM usage u WHERE u.compute_id = c.id), 0),
       COALESCE((SELECT SUM(cost_nanos % 1000000000) FROM usage u WHERE u.compute_id = c.id), 0),
       COALESCE((SELECT MIN(cost_known) FROM usage u WHERE u.compute_id = c.id), 1),
       COALESCE((SELECT MAX(estimated) FROM usage u WHERE u.compute_id = c.id), 0)
FROM compute c JOIN members m ON m.id = c.member_id";

const ARTIFACT_SELECT: &str = "
    SELECT a.id, a.member_id, a.title, a.kind, a.language,
        (SELECT MAX(version) FROM artifact_versions WHERE artifact_id = a.id),
        a.reply_seq, a.created_at, a.updated_at
    FROM artifacts a";

fn artifact_row(r: &rusqlite::Row) -> rusqlite::Result<ArtifactRow> {
    Ok(ArtifactRow {
        id: r.get(0)?,
        member: r.get(1)?,
        title: r.get(2)?,
        kind: r.get(3)?,
        language: r.get(4)?,
        version: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
        message_seq: r.get(6)?,
        created_at: time(r.get(7)?),
        updated_at: time(r.get(8)?),
    })
}

fn compute_row(r: &rusqlite::Row) -> rusqlite::Result<ComputeRow> {
    let approved = match (r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?) {
        (Some(start), Some(end), Some(budget), Some(by), Some(at)) => Some(Approved {
            start,
            end,
            budget,
            by,
            at,
        }),
        _ => None,
    };
    Ok(ComputeRow {
        id: r.get(0)?,
        member: r.get(1)?,
        name: r.get(2)?,
        model: r.get(3)?,
        asked: (r.get(4)?, r.get(5)?, r.get(6)?),
        approved,
        status: r.get(12)?,
        reason: r.get(13)?,
        note: r.get(14)?,
        created: r.get(15)?,
        started: r.get(16)?,
        ended: r.get(17)?,
        prompts: r.get(18)?,
        done: r.get(19)?,
        usage: TotalRow {
            key: None,
            name: None,
            requests: r.get(20)?,
            prompt_tokens: r.get(21)?,
            completion_tokens: r.get(22)?,
            cost_nanos: nanos(r.get(23)?, r.get(24)?),
            cost_known: r.get(25)?,
            estimated: r.get(26)?,
        },
    })
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
                tx.execute("DELETE FROM keys WHERE member_id = ?1", [id])?;
                tx.execute(
                    "UPDATE compute SET status = 'failed', reason = 'member_disabled', ended_at = ?1
                     WHERE member_id = ?2 AND status IN (?3, ?4, ?5)",
                    params![super::limits::now().as_second(), id, COMPUTE_OPEN[0], COMPUTE_OPEN[1], COMPUTE_OPEN[2]],
                )?;
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
            "SELECT id, name, base_url, api_key, local, created_at, pricing FROM sources ORDER BY rowid",
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
            Ok((
                Source {
                    id,
                    name: r.get(1)?,
                    base_url: r.get(2)?,
                    api_key,
                    local: r.get(4)?,
                    created: r.get(5)?,
                    pricing: None,
                },
                r.get::<_, Option<String>>(6)?,
            ))
        })?;
        let mut out = vec![];
        for row in rows {
            let (mut s, stored) = row?;
            s.pricing = pricing(stored).unwrap_or_else(|e| {
                eprintln!("fonsviva org: source {} has no pricing: {e:#}", s.id);
                None
            });
            out.push(s);
        }
        Ok(out)
    }

    /// Replaces a source's pricing, with one `source.update` audit row
    /// naming the fields that changed; false when there is no such source.
    pub fn set_pricing(&mut self, by: &str, id: &str, p: Option<&Pricing>) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let was: Option<Option<String>> = tx
            .query_row("SELECT pricing FROM sources WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(was) = was else {
            return Ok(false);
        };
        let changed = match pricing(was) {
            Ok(old) => costs::changed(old.as_ref(), p),
            Err(e) => {
                eprintln!("fonsviva org: source {id} pricing is replaced: {e:#}");
                let c = costs::changed(None, p);
                if c.is_empty() { vec!["pricing"] } else { c }
            }
        };
        if !changed.is_empty() {
            let stored = p.map(|p| p.json().to_string());
            tx.execute(
                "UPDATE sources SET pricing = ?1 WHERE id = ?2",
                params![stored, id],
            )?;
            audit(
                &tx,
                by,
                "source.update",
                &format!("{id}:{}", changed.join(",")),
            )?;
        }
        tx.commit()?;
        Ok(true)
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
        read_settings(&self.db)
    }

    /// Writes the settings that change, with one audit row naming them.
    pub fn update_settings(&mut self, by: &str, p: &SettingsPatch) -> Result<Settings> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let was = read_settings(&tx)?;
        let mut changed = vec![];
        if let Some(s) = p.students_allowed.filter(|s| *s != was.students_allowed) {
            set(&tx, "students_allowed", flag(s))?;
            changed.push("studentsAllowed");
        }
        if let Some(d) = p.daily_tokens.filter(|d| *d != was.daily_tokens) {
            set(&tx, "daily_tokens", d.map(|n| n.to_string()).as_deref())?;
            changed.push("dailyTokens");
        }
        if let Some(k) = p.keep_threads.filter(|k| *k != was.keep_threads) {
            set(&tx, "keep_threads", flag(k))?;
            changed.push("keepThreads");
        }
        if let Some(a) = p
            .admins_read_threads
            .filter(|a| *a != was.admins_read_threads)
        {
            set(&tx, "admins_read_threads", flag(a))?;
            changed.push("adminsReadThreads");
        }
        if let Some(e) = p
            .electricity_per_kwh
            .filter(|e| *e != was.electricity_per_kwh)
        {
            set(&tx, "electricity_per_kwh", Some(&e.to_string()))?;
            changed.push("electricityPerKwh");
        }
        if let Some(z) = p.timezone.as_ref().filter(|z| **z != was.timezone) {
            set(&tx, "timezone", Some(z))?;
            changed.push("timezone");
        }
        if let Some(u) = &p.search_url {
            let now = u.as_ref().map(|(u, local)| (u.as_str(), *local));
            if now != was.search_url.as_deref().map(|w| (w, was.search_local)) {
                set(&tx, "search_url", now.map(|(u, _)| u))?;
                set(&tx, "search_local", now.and_then(|(_, local)| flag(local)))?;
                changed.push("searchUrl");
            }
        }
        if !changed.is_empty() {
            audit(&tx, by, "settings.update", &changed.join(","))?;
        }
        tx.commit()?;
        self.settings()
    }

    /// A member's role and stored limits; `None` when there is no such
    /// member.
    pub fn limits(&self, member: &str) -> Result<Option<(Role, Limits)>> {
        read_limits(&self.db, member)
    }

    /// Replaces a member's limits, removing the row when all are unset, with
    /// one audit row naming the member and the fields that changed. Returns
    /// the member's role, or `None` when there is no such member.
    pub fn set_limits(&mut self, by: &str, member: &str, l: &Limits) -> Result<Option<Role>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let Some((role, was)) = read_limits_or_default(&tx, member)? else {
            return Ok(None);
        };
        if l.is_empty() {
            tx.execute("DELETE FROM limits WHERE member_id = ?1", [member])?;
        } else {
            let (models, daily, hours) = l.columns();
            tx.execute(
                "INSERT INTO limits (member_id, models, daily_tokens, hours) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (member_id) DO UPDATE SET
                    models = excluded.models, daily_tokens = excluded.daily_tokens, hours = excluded.hours",
                params![member, models, daily, hours],
            )?;
        }
        let changed = l.changed(&was);
        if !changed.is_empty() {
            audit(
                &tx,
                by,
                "limits.update",
                &format!("{member} {}", changed.join(",")),
            )?;
        }
        tx.commit()?;
        Ok(Some(role))
    }

    /// Prompt plus completion tokens the member used since `since`, but
    /// for compute requests.
    pub fn tokens_since(&self, member: &str, since: i64) -> Result<i64> {
        Ok(self.db.query_row(
            "SELECT COALESCE(SUM(COALESCE(prompt_tokens, 0) + COALESCE(completion_tokens, 0)), 0)
             FROM usage WHERE member_id = ?1 AND at >= ?2 AND compute_id IS NULL",
            params![member, since],
            |r| r.get(0),
        )?)
    }

    /// Usage per UTC day, member and model since `since`, for one member or all.
    pub fn usage(&self, since: i64, member: Option<&str>) -> Result<Vec<UsageRow>> {
        let mut q = self.db.prepare(
            "SELECT date(at, 'unixepoch') AS day, member_id, model, COUNT(*),
                    COALESCE(SUM(prompt_tokens), 0), COALESCE(SUM(completion_tokens), 0),
                    MAX(estimated), SUM(cost_nanos / 1000000000), SUM(cost_nanos % 1000000000),
                    MIN(cost_known)
             FROM usage WHERE at >= ?1 AND (?2 IS NULL OR member_id = ?2)
             GROUP BY day, member_id, model ORDER BY day DESC, member_id, model",
        )?;
        let rows = q.query_map(params![since, member], |r| {
            Ok(UsageRow {
                day: r.get(0)?,
                member: r.get(1)?,
                model: r.get(2)?,
                requests: r.get(3)?,
                prompt_tokens: r.get(4)?,
                completion_tokens: r.get(5)?,
                estimated: r.get(6)?,
                cost_nanos: nanos(r.get(7)?, r.get(8)?),
                cost_known: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Usage since `since` per member or model, and its total. Members
    /// with no rows are left out.
    pub fn totals(&self, since: i64, by: By) -> Result<(Vec<TotalRow>, TotalRow)> {
        let (key, name) = match by {
            By::Member => ("u.member_id", "m.name"),
            By::Model => ("u.model", "NULL"),
        };
        let mut q = self.db.prepare(&format!(
            "SELECT {key}, {name}, COUNT(*),
                    COALESCE(SUM(u.prompt_tokens), 0), COALESCE(SUM(u.completion_tokens), 0),
                    SUM(u.cost_nanos / 1000000000), SUM(u.cost_nanos % 1000000000),
                    MIN(u.cost_known), MAX(u.estimated)
             FROM usage u LEFT JOIN members m ON m.id = u.member_id
             WHERE u.at >= ?1 GROUP BY {key} ORDER BY {key}"
        ))?;
        let rows = q.query_map([since], |r| {
            Ok(TotalRow {
                key: r.get(0)?,
                name: r.get(1)?,
                requests: r.get(2)?,
                prompt_tokens: r.get(3)?,
                completion_tokens: r.get(4)?,
                cost_nanos: nanos(r.get(5)?, r.get(6)?),
                cost_known: r.get(7)?,
                estimated: r.get(8)?,
            })
        })?;
        let rows: Vec<TotalRow> = rows.collect::<rusqlite::Result<_>>()?;
        let mut total = TotalRow {
            cost_known: true,
            ..Default::default()
        };
        for r in &rows {
            total.requests += r.requests;
            total.prompt_tokens += r.prompt_tokens;
            total.completion_tokens += r.completion_tokens;
            total.cost_nanos = total.cost_nanos.saturating_add(r.cost_nanos);
            total.cost_known &= r.cost_known;
            total.estimated |= r.estimated;
        }
        Ok((rows, total))
    }

    /// Makes a personal key and returns its id and the key, shown once, or
    /// `None` when the member already holds `MAX_KEYS`.
    pub fn create_key(&mut self, member: &str, name: &str) -> Result<Option<(String, String)>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let held: i64 = tx.query_row(
            "SELECT COUNT(*) FROM keys WHERE member_id = ?1",
            [member],
            |r| r.get(0),
        )?;
        if held >= MAX_KEYS {
            return Ok(None);
        }
        let id = format!("key_{}", random_hex(8));
        let key = format!("fvk_{}", random_hex(32));
        tx.execute(
            "INSERT INTO keys (id, key_sha256, member_id, name, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, sha256_hex(&key), member, name, unix_now() as i64],
        )?;
        tx.commit()?;
        Ok(Some((id, key)))
    }

    pub fn keys(&self, member: &str) -> Result<Vec<KeyRow>> {
        let mut q = self.db.prepare(
            "SELECT id, name, created_at, last_used FROM keys WHERE member_id = ?1 ORDER BY rowid",
        )?;
        let rows = q.query_map([member], |r| {
            Ok(KeyRow {
                id: r.get(0)?,
                name: r.get(1)?,
                created: time(r.get(2)?),
                last_used: r.get::<_, Option<i64>>(3)?.map(time),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Revokes one of the member's own keys; false when they hold no such key.
    pub fn revoke_key(&self, member: &str, id: &str) -> Result<bool> {
        Ok(self.db.execute(
            "DELETE FROM keys WHERE id = ?1 AND member_id = ?2",
            params![id, member],
        )? == 1)
    }

    /// The member a personal key acts as, read fresh, and the key's id.
    /// Marks the key used.
    pub fn key_member(&self, key: &str) -> Result<Option<(Member, String)>> {
        let hash = sha256_hex(key);
        let row: Option<(String, String, String, String, String, bool)> = self
            .db
            .query_row(
                "SELECT k.key_sha256, k.id, m.id, m.name, m.role, m.disabled
                 FROM keys k JOIN members m ON m.id = k.member_id WHERE k.key_sha256 = ?1",
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
        let Some((stored, key_id, id, name, role, disabled)) = row else {
            return Ok(None);
        };
        if !ct_eq(&stored, &hash) || disabled {
            return Ok(None);
        }
        self.db.execute(
            "UPDATE keys SET last_used = ?1 WHERE id = ?2",
            params![unix_now() as i64, key_id],
        )?;
        let role = Role::parse(&role).context("member role")?;
        Ok(Some((Member { id, name, role }, key_id)))
    }

    /// Appends a message to the member's thread and returns its seq.
    /// `status` is the proxy's status on a reply and `None` on the
    /// member's own message.
    pub fn append_message(
        &self,
        member: &str,
        role: &str,
        text: &str,
        model: &str,
        status: Option<u16>,
    ) -> Result<Result<i64, TextRefused>> {
        insert_message(&self.db, member, (role, text, model, status), false)
    }

    /// Up to `limit` of the member's messages before `before`, newest
    /// first, and whether older ones remain.
    pub fn thread(
        &self,
        member: &str,
        before: Option<i64>,
        limit: i64,
    ) -> Result<(Vec<MessageRow>, bool)> {
        let mut q = self.db.prepare(
            "SELECT seq, role, text, model, at, status FROM messages
             WHERE member_id = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = q.query_map(
            params![member, before.unwrap_or(i64::MAX), limit + 1],
            |r| {
                Ok(MessageRow {
                    seq: r.get(0)?,
                    role: r.get(1)?,
                    text: r.get(2)?,
                    model: r.get(3)?,
                    at: time(r.get(4)?),
                    status: r.get(5)?,
                    steps: vec![],
                })
            },
        )?;
        let mut out: Vec<MessageRow> = rows.collect::<rusqlite::Result<_>>()?;
        let more = out.len() as i64 > limit;
        out.truncate(limit as usize);
        let mut steps = self.db.prepare(
            "SELECT name, args, ok FROM message_steps WHERE reply_seq = ?1 ORDER BY idx",
        )?;
        for m in out.iter_mut().filter(|m| m.role == "assistant") {
            let rows = steps.query_map([m.seq], |r| {
                let args: String = r.get(1)?;
                Ok(StepRow {
                    name: r.get(0)?,
                    args: serde_json::from_str(&args).unwrap_or_default(),
                    ok: r.get(2)?,
                })
            })?;
            m.steps = rows.collect::<rusqlite::Result<_>>()?;
        }
        Ok((out, more))
    }

    /// The member's messages that match every word of `q`, newest first.
    pub fn search(&self, member: &str, q: &str, limit: i64) -> Result<Vec<SearchRow>> {
        let mut st = self.db.prepare(
            "SELECT m.seq, m.role, m.at, snippet(messages_fts, 0, char(57344), char(57345), '...', 32)
             FROM messages_fts JOIN messages m ON m.seq = messages_fts.rowid
             WHERE messages_fts MATCH ?1 AND m.member_id = ?2
             ORDER BY m.seq DESC LIMIT ?3",
        )?;
        let rows = st.query_map(params![phrases(q), member, limit], |r| {
            Ok(SearchRow {
                seq: r.get(0)?,
                role: r.get(1)?,
                at: time(r.get(2)?),
                snippet: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The member's messages that `take` keeps, oldest first, as (role,
    /// text), leaving out any that is only whitespace. `take` sees each one's role and byte length, newest first;
    /// `Some(false)` skips it and `None` ends the read, and only the kept
    /// messages' text is read.
    pub fn context(
        &self,
        member: &str,
        mut take: impl FnMut(&str, usize) -> Option<bool>,
    ) -> Result<Vec<(String, String)>> {
        let mut seqs = vec![];
        let space: String = (0..=0x3000)
            .filter_map(char::from_u32)
            .filter(|c| c.is_whitespace())
            .collect();
        let mut sizes = self.db.prepare(
            "SELECT seq, role, octet_length(text) FROM messages
             WHERE member_id = ?1 AND trim(text, ?3) != '' ORDER BY seq DESC LIMIT ?2",
        )?;
        let mut rows = sizes.query(params![member, CONTEXT_SCAN, space])?;
        while let Some(r) = rows.next()? {
            let role: String = r.get(1)?;
            match take(&role, r.get::<_, i64>(2)? as usize) {
                None => break,
                Some(true) => seqs.push(r.get::<_, i64>(0)?),
                Some(false) => {}
            }
        }
        let mut text = self
            .db
            .prepare("SELECT role, text FROM messages WHERE seq = ?1")?;
        seqs.iter()
            .rev()
            .map(|s| Ok(text.query_row([s], |r| Ok((r.get(0)?, r.get(1)?)))?))
            .collect()
    }

    /// Deletes one of the member's own messages; false when they hold none
    /// with that seq.
    pub fn delete_message(&self, member: &str, seq: i64) -> Result<bool> {
        Ok(self.db.execute(
            "DELETE FROM messages WHERE seq = ?1 AND member_id = ?2",
            params![seq, member],
        )? == 1)
    }

    /// Deletes the member's whole thread. `merge_step` and `checkpoint` then
    /// clear its text from the index and the log.
    pub fn delete_thread(&self, member: &str) -> Result<()> {
        self.db
            .execute("DELETE FROM messages WHERE member_id = ?1", [member])?;
        Ok(())
    }

    /// One bounded merge of the FTS index; false when it did no work. Run
    /// until then, merges drop deleted messages' words, and `secure_delete`
    /// overwrites the pages they held.
    pub fn merge_step(&self) -> Result<bool> {
        let was = self.db.total_changes();
        self.db.execute(
            "INSERT INTO messages_fts (messages_fts, rank) VALUES ('merge', -16)",
            [],
        )?;
        Ok(self.db.total_changes() - was >= 2)
    }

    /// Copies the write-ahead log into org.db and empties it, without
    /// waiting on readers; false when a reader kept it from finishing.
    pub fn checkpoint(&self) -> Result<bool> {
        self.db.busy_timeout(std::time::Duration::ZERO)?;
        let busy = self
            .db
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                r.get::<_, i64>(0)
            });
        self.db.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(busy? == 0)
    }

    /// Notes that a delete of the member's answered busy before its text
    /// was cleared, so a later retry of it runs the cleanup.
    pub fn mark_cleanup(&self, member: &str) -> Result<()> {
        set(&self.db, &format!("cleanup:{member}"), Some("1"))
    }

    /// Whether any delete's cleanup is pending, and whether the member's is.
    pub fn cleanup_pending(&self, member: &str) -> Result<(bool, bool)> {
        let any = self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM org WHERE key GLOB 'cleanup:*')",
            [],
            |r| r.get(0),
        )?;
        Ok((
            any,
            setting(&self.db, &format!("cleanup:{member}"))?.is_some(),
        ))
    }

    /// Ends every pending cleanup, once one found nothing left to clear.
    pub fn clear_cleanups(&self) -> Result<()> {
        self.db
            .execute("DELETE FROM org WHERE key GLOB 'cleanup:*'", [])?;
        Ok(())
    }

    /// Whether the member exists.
    pub fn has_member(&self, member: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS (SELECT 1 FROM members WHERE id = ?1)",
            [member],
            |r| r.get(0),
        )?)
    }

    /// Writes the audit row for an admin reading a member's thread.
    pub fn audit_thread_read(&self, by: &str, member: &str) -> Result<()> {
        audit(&self.db, by, "thread.read", member)
    }

    /// Appends a reply to the member's thread while the message `after` it
    /// answers is still there, with its tool calls, and ties the artifacts
    /// its send made to it, all in one commit, and returns its seq. A reply
    /// with no text is kept only with steps or artifacts; `None` when it
    /// isn't kept or `after` is gone.
    pub fn append_reply(
        &mut self,
        member: &str,
        after: i64,
        (text, model, status): (&str, &str, u16),
        (steps, made): (&[Step], &[String]),
    ) -> Result<Option<i64>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let there: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE seq = ?1 AND member_id = ?2)",
            params![after, member],
            |r| r.get(0),
        )?;
        if !there {
            return Ok(None);
        }
        let blank = !steps.is_empty() || !made.is_empty();
        let reply = ("assistant", text, model, Some(status));
        let Ok(reply) = insert_message(&tx, member, reply, blank)? else {
            return Ok(None);
        };
        for (i, s) in steps.iter().enumerate() {
            tx.execute(
                "INSERT INTO message_steps (reply_seq, idx, name, args, ok, result)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![reply, i as i64, s.name, s.args, s.ok, s.result],
            )?;
        }
        for id in made {
            tx.execute(
                "UPDATE artifacts SET reply_seq = ?1 WHERE id = ?2 AND reply_seq IS NULL",
                params![reply, id],
            )?;
        }
        tx.commit()?;
        Ok(Some(reply))
    }

    /// Makes an artifact at version 1 and returns its id.
    pub fn make_artifact(
        &mut self,
        member: &str,
        title: &str,
        kind: &str,
        language: Option<&str>,
        content: &str,
    ) -> Result<Result<String, ArtifactRefused>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let n: i64 = tx.query_row(
            "SELECT COUNT(*) FROM artifacts WHERE member_id = ?1",
            [member],
            |r| r.get(0),
        )?;
        if n >= MAX_ARTIFACTS {
            return Ok(Err(ArtifactRefused::TooMany));
        }
        let id = format!("art_{}", random_hex(8));
        let now = unix_now() as i64;
        tx.execute(
            "INSERT INTO artifacts (id, member_id, title, kind, language, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
            params![id, member, title, kind, language, now],
        )?;
        tx.execute(
            "INSERT INTO artifact_versions (artifact_id, version, content, created_at)
             VALUES (?1, 1, ?2, ?3)",
            params![id, content, now],
        )?;
        tx.commit()?;
        Ok(Ok(id))
    }

    /// Adds the next version of one of the member's own artifacts and
    /// returns the artifact as it is now.
    pub fn update_artifact(
        &mut self,
        member: &str,
        id: &str,
        content: &str,
        title: Option<&str>,
    ) -> Result<Result<ArtifactRow, ArtifactRefused>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let owned: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM artifacts WHERE id = ?1 AND member_id = ?2)",
            params![id, member],
            |r| r.get(0),
        )?;
        if !owned {
            return Ok(Err(ArtifactRefused::NotFound));
        }
        let last: i64 = tx.query_row(
            "SELECT MAX(version) FROM artifact_versions WHERE artifact_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        if last >= MAX_VERSIONS {
            return Ok(Err(ArtifactRefused::TooManyVersions));
        }
        let now = unix_now() as i64;
        tx.execute(
            "INSERT INTO artifact_versions (artifact_id, version, content, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, last + 1, content, now],
        )?;
        tx.execute(
            "UPDATE artifacts SET title = COALESCE(?1, title), updated_at = ?2 WHERE id = ?3",
            params![title, now, id],
        )?;
        let row = tx.query_row(
            &format!("{ARTIFACT_SELECT} WHERE a.id = ?1"),
            [id],
            artifact_row,
        )?;
        tx.commit()?;
        Ok(Ok(row))
    }

    /// The member's artifacts, newest first.
    pub fn artifacts(&self, member: &str) -> Result<Vec<ArtifactRow>> {
        let mut q = self.db.prepare(&format!(
            "{ARTIFACT_SELECT} WHERE a.member_id = ?1 ORDER BY a.created_at DESC, a.rowid DESC"
        ))?;
        let rows = q.query_map([member], artifact_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn artifact(&self, id: &str) -> Result<Option<ArtifactRow>> {
        Ok(self
            .db
            .query_row(
                &format!("{ARTIFACT_SELECT} WHERE a.id = ?1"),
                [id],
                artifact_row,
            )
            .optional()?)
    }

    pub fn artifact_versions(&self, id: &str) -> Result<Vec<VersionRow>> {
        let mut q = self.db.prepare(
            "SELECT version, octet_length(content), created_at FROM artifact_versions
             WHERE artifact_id = ?1 ORDER BY version",
        )?;
        let rows = q.query_map([id], |r| {
            Ok(VersionRow {
                version: r.get(0)?,
                bytes: r.get(1)?,
                created_at: time(r.get(2)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The content of an artifact's version, its newest for `None`.
    pub fn artifact_content(&self, id: &str, version: Option<i64>) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row(
                "SELECT content FROM artifact_versions WHERE artifact_id = ?1
                 AND version = COALESCE(?2, (SELECT MAX(version) FROM artifact_versions WHERE artifact_id = ?1))",
                params![id, version],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Deletes one of the member's own artifacts with every version; false
    /// when they hold none with that id. `checkpoint` then clears its text
    /// from the log.
    pub fn delete_artifact(&mut self, member: &str, id: &str) -> Result<bool> {
        let tx = self.db.transaction()?;
        tx.execute(
            "DELETE FROM artifact_versions WHERE artifact_id = (
                 SELECT id FROM artifacts WHERE id = ?1 AND member_id = ?2)",
            params![id, member],
        )?;
        let found = tx.execute(
            "DELETE FROM artifacts WHERE id = ?1 AND member_id = ?2",
            params![id, member],
        )? == 1;
        tx.commit()?;
        Ok(found)
    }

    /// Writes the audit row for an admin reading a member's artifact.
    pub fn audit_artifact_read(&self, by: &str, id: &str) -> Result<()> {
        audit(&self.db, by, "artifact.read", id)
    }

    /// Files a compute request and its prompts; `None` when the member
    /// already holds `MAX_OPEN_COMPUTE` open ones or keeps
    /// `MAX_KEPT_COMPUTE` in all.
    pub fn file_compute(&mut self, c: &NewCompute) -> Result<Option<String>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (open, kept): (i64, i64) = tx.query_row(
            "SELECT COALESCE(SUM(status IN (?2, ?3, ?4)), 0), COUNT(*) FROM compute WHERE member_id = ?1",
            params![c.member, COMPUTE_OPEN[0], COMPUTE_OPEN[1], COMPUTE_OPEN[2]],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if open >= MAX_OPEN_COMPUTE || kept >= MAX_KEPT_COMPUTE {
            return Ok(None);
        }
        let id = format!("cmp_{}", random_hex(8));
        tx.execute(
            "INSERT INTO compute (id, member_id, model, start_asked, end_asked, max_asked, status, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
            params![id, c.member, c.model, c.start, c.end, c.budget, c.note, unix_now() as i64],
        )?;
        for (i, p) in c.prompts.iter().enumerate() {
            tx.execute(
                "INSERT INTO compute_prompts (compute_id, idx, prompt) VALUES (?1, ?2, ?3)",
                params![id, i as i64, p],
            )?;
        }
        tx.commit()?;
        Ok(Some(id))
    }

    pub fn compute(&self, id: &str) -> Result<Option<ComputeRow>> {
        let row = self
            .db
            .query_row(
                &format!("{COMPUTE_SELECT} WHERE c.id = ?1"),
                [id],
                compute_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Compute requests newest first, of one member or all, in one status
    /// or any.
    pub fn computes(&self, member: Option<&str>, status: Option<&str>) -> Result<Vec<ComputeRow>> {
        let mut q = self.db.prepare(&format!(
            "{COMPUTE_SELECT} WHERE (?1 IS NULL OR c.member_id = ?1) AND (?2 IS NULL OR c.status = ?2)
             ORDER BY c.created_at DESC, c.rowid DESC"
        ))?;
        let rows = q.query_map(params![member, status], compute_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Approves a pending request with this window and budget, with its
    /// audit row; false when it is no longer pending.
    pub fn approve_compute(
        &mut self,
        by: &str,
        id: &str,
        (start, end, max): (i64, i64, i64),
    ) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE compute SET status = 'approved', start = ?1, end = ?2, max_tokens = ?3,
                    approved_by = ?4, approved_at = ?5
             WHERE id = ?6 AND status = 'pending'",
            params![start, end, max, by, unix_now() as i64, id],
        )?;
        if n == 1 {
            audit(&tx, by, "compute.approve", id)?;
        }
        tx.commit()?;
        Ok(n == 1)
    }

    /// Denies a pending request, with its audit row; false when it is no
    /// longer pending.
    pub fn deny_compute(&mut self, by: &str, id: &str, reason: Option<&str>) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE compute SET status = 'denied', reason = ?1, ended_at = ?2
             WHERE id = ?3 AND status = 'pending'",
            params![reason, unix_now() as i64, id],
        )?;
        if n == 1 {
            audit(&tx, by, "compute.deny", id)?;
        }
        tx.commit()?;
        Ok(n == 1)
    }

    /// Cancels an open request; false when it has finished. An admin's
    /// cancel of another member's request is audited.
    pub fn cancel_compute(&mut self, by: &str, id: &str, owner: &str) -> Result<bool> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let n = tx.execute(
            "UPDATE compute SET status = 'cancelled', ended_at = ?1
             WHERE id = ?2 AND status IN (?3, ?4, ?5)",
            params![
                unix_now() as i64,
                id,
                COMPUTE_OPEN[0],
                COMPUTE_OPEN[1],
                COMPUTE_OPEN[2]
            ],
        )?;
        if n == 1 && by != owner {
            audit(&tx, by, "compute.cancel", id)?;
        }
        tx.commit()?;
        Ok(n == 1)
    }

    /// Deletes one of the member's finished requests with its prompts and
    /// results: `None` when they hold no such request, false when it is
    /// still open. `checkpoint` then clears its text from the log.
    pub fn delete_compute(&mut self, member: &str, id: &str) -> Result<Option<bool>> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM compute WHERE id = ?1 AND member_id = ?2",
                params![id, member],
                |r| r.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Ok(None);
        };
        if COMPUTE_OPEN.contains(&status.as_str()) {
            return Ok(Some(false));
        }
        tx.execute("DELETE FROM compute_results WHERE compute_id = ?1", [id])?;
        tx.execute("DELETE FROM compute_prompts WHERE compute_id = ?1", [id])?;
        tx.execute("DELETE FROM compute WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(Some(true))
    }

    /// A request's answered prompts in order, each with its prompt.
    pub fn compute_results(&self, id: &str) -> Result<Vec<ComputeResult>> {
        let mut q = self.db.prepare(
            "SELECT r.idx, p.prompt, r.text, r.status, r.prompt_tokens, r.completion_tokens, r.cost_nanos
             FROM compute_results r JOIN compute_prompts p ON p.compute_id = r.compute_id AND p.idx = r.idx
             WHERE r.compute_id = ?1 ORDER BY r.idx",
        )?;
        let rows = q.query_map([id], |r| {
            Ok(ComputeResult {
                index: r.get(0)?,
                prompt: r.get(1)?,
                text: r.get(2)?,
                status: r.get(3)?,
                prompt_tokens: r.get(4)?,
                completion_tokens: r.get(5)?,
                cost_nanos: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Writes the audit row for an admin reading a member's compute results.
    pub fn audit_compute_read(&self, by: &str, id: &str) -> Result<()> {
        audit(&self.db, by, "compute.read", id)
    }

    /// The member with this id and whether they are disabled.
    pub fn member_state(&self, id: &str) -> Result<Option<(Member, bool)>> {
        let row: Option<(String, String, bool)> = self
            .db
            .query_row(
                "SELECT name, role, disabled FROM members WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(name, role, disabled)| {
            let role = Role::parse(&role).context("member role")?;
            let m = Member {
                id: id.to_string(),
                name,
                role,
            };
            Ok((m, disabled))
        })
        .transpose()
    }

    /// Running requests, then approved ones, each in start order.
    pub fn compute_due(&self) -> Result<Vec<Due>> {
        let mut q = self.db.prepare(
            "SELECT id, member_id, status = 'running', start, end FROM compute
             WHERE status IN ('approved', 'running')
             ORDER BY status = 'running' DESC, start, created_at, rowid",
        )?;
        let rows = q.query_map([], |r| {
            Ok(Due {
                id: r.get(0)?,
                member: r.get(1)?,
                running: r.get(2)?,
                start: r.get(3)?,
                end: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Starts an approved request; false when it is no longer approved.
    pub fn start_compute(&self, id: &str, now: i64) -> Result<bool> {
        let n = self.db.execute(
            "UPDATE compute SET status = 'running', started_at = COALESCE(started_at, ?1)
             WHERE id = ?2 AND status = 'approved'",
            params![now, id],
        )?;
        Ok(n == 1)
    }

    /// Ends an approved or running request in `status`; false when it has
    /// already ended.
    pub fn end_compute(
        &self,
        id: &str,
        status: &str,
        reason: Option<&str>,
        now: i64,
    ) -> Result<bool> {
        let n = self.db.execute(
            "UPDATE compute SET status = ?1, reason = ?2, ended_at = ?3
             WHERE id = ?4 AND status IN ('approved', 'running')",
            params![status, reason, now, id],
        )?;
        Ok(n == 1)
    }

    /// The first prompt with no answer, or one a stop left at 499.
    pub fn compute_next(&self, id: &str) -> Result<Option<(i64, String)>> {
        let next = self
            .db
            .query_row(
                "SELECT p.idx, p.prompt FROM compute_prompts p
                 LEFT JOIN compute_results r ON r.compute_id = p.compute_id AND r.idx = p.idx
                 WHERE p.compute_id = ?1 AND (r.idx IS NULL OR r.status = ?2)
                 ORDER BY p.idx LIMIT 1",
                params![id, chat::CLIENT_GONE],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(next)
    }

    /// Keeps a prompt's answer, over one a stop left.
    pub fn put_answer(&self, id: &str, idx: i64, a: &Answer) -> Result<()> {
        self.db.execute(
            "INSERT OR REPLACE INTO compute_results
                 (compute_id, idx, text, status, prompt_tokens, completion_tokens, cost_nanos)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7 WHERE EXISTS (SELECT 1 FROM compute WHERE id = ?1)",
            params![
                id,
                idx,
                a.text,
                a.status,
                a.prompt_tokens,
                a.completion_tokens,
                a.cost_nanos
            ],
        )?;
        Ok(())
    }

    /// Writes a usage row with its cost at the source's pricing as it is
    /// now, and returns the cost. A row naming only the source gets no
    /// model's override.
    pub fn record_usage(&self, u: &Usage) -> Result<i64> {
        let (name, model) = match u.model.split_once('/') {
            Some((s, m)) => (s, Some(m)),
            None => (u.model, None),
        };
        let stored: Option<Option<String>> = self
            .db
            .query_row("SELECT pricing FROM sources WHERE name = ?1", [name], |r| {
                r.get(0)
            })
            .optional()?;
        let answered = (200..300).contains(&u.status) || u.status == 499;
        let priced =
            pricing(stored.flatten()).and_then(|p| Ok((p, self.settings()?.electricity_per_kwh)));
        let (cost, known) = match priced {
            Ok((p, kwh)) => costs::cost(
                p.as_ref(),
                model,
                u.prompt_tokens.unwrap_or(0),
                u.completion_tokens.unwrap_or(0),
                u.ms,
                kwh,
                answered,
            ),
            Err(e) => {
                eprintln!("fonsviva org: no cost for a usage row: {e:#}");
                (0, false)
            }
        };
        self.db.execute(
            "INSERT INTO usage (at, member_id, key_id, model, status, prompt_tokens, completion_tokens, ms, estimated, cost_nanos, cost_known, compute_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                unix_now() as i64,
                u.member,
                u.key,
                u.model,
                u.status,
                u.prompt_tokens,
                u.completion_tokens,
                u.ms,
                u.estimated,
                cost,
                known,
                u.compute
            ],
        )?;
        Ok(cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saturated_costs_sum_without_an_error() {
        let d = tempfile::tempdir().unwrap();
        let mut store = Store::create(&d.path().join("org.db"), "https://org.example").unwrap();
        let link = store.new_admin_link("cli").unwrap();
        let (m, _) = store.join(&link, "Ada").unwrap().unwrap();
        for _ in 0..2 {
            store
                .db
                .execute(
                    "INSERT INTO usage (at, member_id, model, status, ms, cost_nanos, cost_known)
                     VALUES (?1, ?2, 's/a', 200, 1, ?3, 1)",
                    params![unix_now() as i64, m.id, i64::MAX],
                )
                .unwrap();
        }
        let rows = store.usage(0, None).unwrap();
        assert_eq!(rows[0].cost_nanos, i64::MAX);
        let (rows, total) = store.totals(0, By::Member).unwrap();
        assert_eq!(rows[0].cost_nanos, i64::MAX);
        assert_eq!(total.cost_nanos, i64::MAX);
    }

    #[test]
    fn a_pricing_that_no_longer_reads_still_writes_the_row() {
        let d = tempfile::tempdir().unwrap();
        let mut store = Store::create(&d.path().join("org.db"), "https://org.example").unwrap();
        let link = store.new_admin_link("cli").unwrap();
        let (m, _) = store.join(&link, "Ada").unwrap().unwrap();
        store
            .db
            .execute(
                "INSERT INTO sources (id, name, base_url, local, created_at, pricing)
                 VALUES ('src_s', 's', 'http://127.0.0.1:1/v1', 1, 0, '{\"watts\": 0}')",
                [],
            )
            .unwrap();
        let u = Usage {
            member: &m.id,
            key: None,
            model: "s/a",
            status: 200,
            prompt_tokens: Some(40),
            completion_tokens: Some(40),
            estimated: false,
            ms: 5,
            compute: None,
        };
        store.record_usage(&u).unwrap();
        assert_eq!(store.tokens_since(&m.id, 0).unwrap(), 80);
        let rows = store.usage(0, None).unwrap();
        assert_eq!((rows[0].cost_nanos, rows[0].cost_known), (0, false));
    }

    #[test]
    fn a_failed_request_is_charged_no_power() {
        let d = tempfile::tempdir().unwrap();
        let mut store = Store::create(&d.path().join("org.db"), "https://org.example").unwrap();
        let link = store.new_admin_link("cli").unwrap();
        let (m, _) = store.join(&link, "Ada").unwrap().unwrap();
        store
            .db
            .execute(
                "INSERT INTO sources (id, name, base_url, local, created_at, pricing)
                 VALUES ('src_s', 's', 'http://127.0.0.1:1/v1', 1, 0, '{\"watts\": 300}')",
                [],
            )
            .unwrap();
        for status in [502, 200] {
            let u = Usage {
                member: &m.id,
                key: None,
                model: "s",
                status,
                prompt_tokens: None,
                completion_tokens: None,
                estimated: false,
                ms: 2000,
                compute: None,
            };
            store.record_usage(&u).unwrap();
        }
        let got: Vec<(u16, i64)> = store
            .db
            .prepare("SELECT status, cost_nanos FROM usage ORDER BY seq")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(got, [(502, 0), (200, 25_000)]);
    }

    #[test]
    fn a_keyed_source_prints_no_key() {
        let s = Source {
            id: "src_a".into(),
            name: "a".into(),
            base_url: "https://api.example.com/v1".into(),
            api_key: SourceKey::Key("sk-debug-secret-91c2".into()),
            local: false,
            created: 0,
            pricing: None,
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

    #[test]
    fn case_4_a_message_over_256_kib_or_with_no_text_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let mut store = Store::create(&d.path().join("org.db"), "https://org.example").unwrap();
        let link = store.new_admin_link("cli").unwrap();
        let (m, _) = store.join(&link, "Ada").unwrap().unwrap();
        let add = |t: &str| {
            store
                .append_message(&m.id, "user", t, "stub/m", None)
                .unwrap()
        };
        assert_eq!(add(&"a".repeat(TEXT_LIMIT + 1)), Err(TextRefused::TooLarge));
        for empty in ["", " ", "\n\t  "] {
            assert_eq!(add(empty), Err(TextRefused::Empty));
        }
        assert_eq!(add("\u{E000}\u{E001} "), Err(TextRefused::Empty));
        assert_eq!(store.thread(&m.id, None, 50).unwrap().0.len(), 0);
        let marked = add("a\u{E000}b\u{E001}c").unwrap();
        let (rows, _) = store.thread(&m.id, None, 50).unwrap();
        assert_eq!((rows[0].seq, rows[0].text.as_str()), (marked, "abc"));
        store.delete_message(&m.id, marked).unwrap();
        let seq = add(&"a".repeat(TEXT_LIMIT)).unwrap();
        let (rows, more) = store.thread(&m.id, None, 50).unwrap();
        assert_eq!((rows.len(), rows[0].seq, more), (1, seq, false));
    }
}
