//! Per-person limits: which models a member may use and how many tokens a
//! day (docs/ARCHITECTURE.md, Limits).

use anyhow::{Context, Result};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Extension, Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use super::chat::split_model;
use super::http::{Caller, OrgState, Session, St, err, internal};
use super::store::{Member, Role, Settings};

const MAX_MODELS: usize = 200;
/// The longest model id a list may hold, as long as a chat may name.
const MODEL_LIMIT: usize = 200;
const MAX_DAILY: u64 = 1_000_000_000;

/// A member's limits as stored; `None` leaves each one unset.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Limits {
    pub models: Option<Vec<String>>,
    pub daily_tokens: Option<u64>,
}

impl Limits {
    /// A `PUT` body: an object of `models`, `dailyTokens` and `hours`, each
    /// null or absent when unset. `None` when any part is a bad shape;
    /// `hours` is not taken yet.
    pub fn parse(v: &Value) -> Option<Limits> {
        let o = v.as_object()?;
        if o.keys()
            .any(|k| !["models", "dailyTokens", "hours"].contains(&k.as_str()))
        {
            return None;
        }
        let set = |k: &str| o.get(k).filter(|v| !v.is_null());
        if set("hours").is_some() {
            return None;
        }
        let models = match set("models") {
            None => None,
            Some(v) => {
                let list = v.as_array().filter(|l| l.len() <= MAX_MODELS)?;
                let ids = list.iter().map(|m| {
                    m.as_str()
                        .filter(|m| m.len() <= MODEL_LIMIT && split_model(m).is_some())
                        .map(str::to_string)
                });
                Some(ids.collect::<Option<Vec<_>>>()?)
            }
        };
        let daily = match set("dailyTokens") {
            None => None,
            Some(v) => Some(v.as_u64().filter(|n| (1..=MAX_DAILY).contains(n))?),
        };
        Some(Limits {
            models,
            daily_tokens: daily,
        })
    }

    pub fn json(&self) -> Value {
        json!({"models": self.models, "dailyTokens": self.daily_tokens, "hours": null})
    }

    pub fn is_empty(&self) -> bool {
        *self == Limits::default()
    }

    /// The names of the fields that differ from `was`.
    pub fn changed(&self, was: &Limits) -> Vec<&'static str> {
        let mut out = vec![];
        if self.models != was.models {
            out.push("models");
        }
        if self.daily_tokens != was.daily_tokens {
            out.push("dailyTokens");
        }
        out
    }

    /// The `models` and `daily_tokens` columns of a `limits` row.
    pub fn columns(&self) -> (Option<String>, Option<i64>) {
        let models = self.models.as_ref().map(|m| json!(m).to_string());
        (models, self.daily_tokens.map(|n| n as i64))
    }

    pub fn from_columns(models: Option<String>, daily: Option<i64>) -> Result<Limits> {
        let parsed = Limits::parse(&json!({
            "models": models.as_deref().map(serde_json::from_str::<Value>).transpose()?,
            "dailyTokens": daily,
        }));
        parsed.context("stored limits")
    }
}

/// The limits that hold for a member: none for an admin; for anyone else
/// their own, with the org's daily limit for users and students who have
/// none of their own.
pub fn effective(role: Role, own: &Limits, s: &Settings) -> Limits {
    if role == Role::Admin {
        return Limits::default();
    }
    let default = matches!(role, Role::User | Role::Student)
        .then_some(s.daily_tokens)
        .flatten();
    let daily = own.daily_tokens.or(default);
    Limits {
        models: own.models.clone(),
        daily_tokens: daily,
    }
}

/// The member's limits as they hold now, and the org's settings.
pub fn of(st: &OrgState, m: &Member) -> Result<(Limits, Settings)> {
    let store = st.store();
    let s = store.settings()?;
    let own = store.limits(&m.id)?.map(|(_, l)| l).unwrap_or_default();
    Ok((effective(m.role, &own, &s), s))
}

fn member_reply(own: &Limits, role: Role, s: &Settings) -> Value {
    let mut v = own.json();
    v["effective"] = effective(role, own, s).json();
    v
}

pub async fn member_limits(State(st): State<St>, UrlPath(id): UrlPath<String>) -> Response {
    let store = st.store();
    match (store.limits(&id), store.settings()) {
        (Ok(Some((role, own))), Ok(s)) => Json(member_reply(&own, role, &s)).into_response(),
        (Ok(None), _) => err(StatusCode::NOT_FOUND, "not_found"),
        (Err(e), _) | (_, Err(e)) => internal(e),
    }
}

pub async fn put_limits(
    State(st): State<St>,
    Extension(s): Extension<Session>,
    UrlPath(id): UrlPath<String>,
    b: Bytes,
) -> Response {
    let Some(l) = serde_json::from_slice::<Value>(&b)
        .ok()
        .as_ref()
        .and_then(Limits::parse)
    else {
        return err(StatusCode::BAD_REQUEST, "bad_request");
    };
    let mut store = st.store();
    let set = store.set_limits(&s.member.id, &id, &l);
    match (set, store.settings()) {
        (Ok(Some(role)), Ok(settings)) => Json(member_reply(&l, role, &settings)).into_response(),
        (Ok(None), _) => err(StatusCode::NOT_FOUND, "not_found"),
        (Err(e), _) | (_, Err(e)) => internal(e),
    }
}

/// The caller's limits and whether they may chat right now.
pub async fn my_limits(State(st): State<St>, Extension(c): Extension<Caller>) -> Response {
    let (l, s) = match of(&st, &c.member) {
        Ok(f) => f,
        Err(e) => return internal(e),
    };
    let off = c.member.role == Role::Student && !s.students_allowed;
    let reason = off.then_some("students_off");
    Json(json!({
        "effective": l.json(),
        "now": {"allowed": reason.is_none(), "reason": reason, "nextOpen": null},
    }))
    .into_response()
}
