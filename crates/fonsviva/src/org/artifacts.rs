//! Artifacts the org brain made for a member: listed, read and served to
//! their owner, and to an admin only while admins read threads
//! (docs/ARCHITECTURE.md, Org brain).

use axum::Json;
use axum::extract::{Extension, Path as UrlPath, Query, State};
use axum::http::{HeaderName, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;

use super::http::{Caller, St, after_delete, err, internal};
use super::store::{ArtifactRow, Role, Store};

/// An html artifact's scripts run in an opaque origin with no cookie, no
/// storage, no Fonsviva credentials and no subresource fetches. CSP can't
/// stop the page navigating its own frame, so the panel that shows it frames
/// it with its own `sandbox` (no `allow-top-navigation`, no
/// `allow-same-origin`) and opens it only when the member asks.
const HTML_CSP: &str = "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:";

/// The artifact when the caller may read it: the owner, or an admin while
/// admins read threads. Anyone else gets the answer a missing id gets.
fn visible(store: &Store, c: &Caller, id: &str) -> anyhow::Result<Result<ArtifactRow, Response>> {
    let Some(a) = store.artifact(id)? else {
        return Ok(Err(err(StatusCode::NOT_FOUND, "not_found")));
    };
    if a.member == c.member.id {
        return Ok(Ok(a));
    }
    if c.member.role != Role::Admin {
        return Ok(Err(err(StatusCode::NOT_FOUND, "not_found")));
    }
    if !store.settings()?.admins_read_threads {
        return Ok(Err(err(StatusCode::FORBIDDEN, "threads_private")));
    }
    store.audit_artifact_read(&c.member.id, id)?;
    Ok(Ok(a))
}

pub async fn list(State(st): State<St>, Extension(c): Extension<Caller>) -> Response {
    match st.store().artifacts(&c.member.id) {
        Ok(a) => Json(json!({"artifacts": a})).into_response(),
        Err(e) => internal(e),
    }
}

pub async fn one(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let store = st.store();
    let read = (|| {
        let a = match visible(&store, &c, &id)? {
            Ok(a) => a,
            Err(r) => return Ok(r),
        };
        let mut v = serde_json::to_value(&a)?;
        v["versions"] = serde_json::to_value(store.artifact_versions(&id)?)?;
        Ok(Json(v).into_response())
    })();
    read.unwrap_or_else(internal)
}

pub async fn content(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let version = match q.get("version").map(|v| v.parse::<i64>()) {
        None => None,
        Some(Ok(n)) if n > 0 => Some(n),
        Some(_) => return err(StatusCode::BAD_REQUEST, "bad_request"),
    };
    let store = st.store();
    let read = (|| {
        let a = match visible(&store, &c, &id)? {
            Ok(a) => a,
            Err(r) => return Ok(r),
        };
        let Some(text) = store.artifact_content(&id, version)? else {
            return Ok(err(StatusCode::NOT_FOUND, "not_found"));
        };
        Ok(serve(&a.kind, text))
    })();
    read.unwrap_or_else(internal)
}

/// A version's content with the headers every content answer carries.
fn serve(kind: &str, text: String) -> Response {
    let html = kind == "html";
    let ctype = if html {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    let mut r = (
        [
            (header::CONTENT_TYPE, ctype),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (
                HeaderName::from_static("cross-origin-resource-policy"),
                "same-origin",
            ),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_DISPOSITION, "inline"),
        ],
        text,
    )
        .into_response();
    if html {
        r.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            header::HeaderValue::from_static(HTML_CSP),
        );
    }
    r
}

pub async fn remove(
    State(st): State<St>,
    Extension(c): Extension<Caller>,
    UrlPath(id): UrlPath<String>,
) -> Response {
    let deleted = st.store().delete_artifact(&c.member.id, &id);
    after_delete(&st, &c.member.id, deleted).await
}
