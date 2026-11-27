//! The org brain's tools: the definitions a thread send offers the model,
//! fixed server text, and each call run as server code for the member who
//! sent it (docs/ARCHITECTURE.md, Org brain).

use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashSet;

use super::http::St;
use super::store::{ArtifactRefused, Settings};
use super::web;

/// The most of a tool result, or of a call's shown args, a reply keeps.
pub const RESULT_LIMIT: usize = 64 << 10;
const TITLE_LIMIT: usize = 200;
const LANGUAGE_LIMIT: usize = 40;
const CONTENT_LIMIT: usize = 1 << 20;
const KINDS: &[&str] = &["html", "markdown", "code"];
const QUERY_LIMIT: usize = 400;
pub const URL_LIMIT: usize = 2048;

/// Whether a shown arg's value is kept.
type Shown = fn(&Value) -> bool;

pub(super) struct Tool {
    name: &'static str,
    /// The definition sent upstream.
    spec: &'static str,
    /// Whether the org's settings offer it.
    on: fn(&Settings) -> bool,
    /// The args a thread shows and keeps of a call, each kept only when
    /// its test passes.
    shown: &'static [(&'static str, Shown)],
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "make_artifact",
        spec: MAKE_ARTIFACT,
        on: |_| true,
        shown: &[("kind", is_kind)],
    },
    Tool {
        name: "update_artifact",
        spec: UPDATE_ARTIFACT,
        on: |_| true,
        shown: &[("id", is_artifact_id)],
    },
    Tool {
        name: "web_search",
        spec: WEB_SEARCH,
        on: |s| s.search_url.is_some(),
        shown: &[("query", is_query)],
    },
    Tool {
        name: "read_page",
        spec: READ_PAGE,
        on: |s| s.search_url.is_some(),
        shown: &[("url", is_page)],
    },
];

const MAKE_ARTIFACT: &str = r#"{"type":"function","function":{"name":"make_artifact","description":"Make an artifact the member opens beside the thread: an HTML page, a Markdown document or a code file. Returns its id and version.","parameters":{"type":"object","properties":{"title":{"type":"string","description":"1 to 200 characters."},"kind":{"type":"string","enum":["html","markdown","code"]},"language":{"type":"string","description":"For code only: the language, such as python or c++."},"content":{"type":"string","description":"The whole artifact, at most 1 MiB."}},"required":["title","kind","content"],"additionalProperties":false}}}"#;

const WEB_SEARCH: &str = r#"{"type":"function","function":{"name":"web_search","description":"Search the web through the org's search service. Returns up to 8 results, each with a title, url and snippet.","parameters":{"type":"object","properties":{"query":{"type":"string","description":"1 to 400 characters."}},"required":["query"],"additionalProperties":false}}}"#;

const READ_PAGE: &str = r#"{"type":"function","function":{"name":"read_page","description":"Read a web page's text: a url from this send's web_search results or one the member wrote in their message. Returns its title, its url after redirects and its text, cut so the answer is at most 64 KiB.","parameters":{"type":"object","properties":{"url":{"type":"string"}},"required":["url"],"additionalProperties":false}}}"#;

const UPDATE_ARTIFACT: &str = r#"{"type":"function","function":{"name":"update_artifact","description":"Replace the content of an artifact made earlier with a new version, and its title when given. Returns its id and new version.","parameters":{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string","description":"The whole new content, at most 1 MiB."},"title":{"type":"string","description":"1 to 200 characters."}},"required":["id","content"],"additionalProperties":false}}}"#;

fn is_kind(v: &Value) -> bool {
    v.as_str().is_some_and(|k| KINDS.contains(&k))
}

/// Whether `v` has the shape of the ids `make_artifact` gives.
fn is_artifact_id(v: &Value) -> bool {
    v.as_str()
        .and_then(|id| id.strip_prefix("art_"))
        .is_some_and(|h| h.len() == 16 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

fn query_ok(q: &str) -> bool {
    (1..=QUERY_LIMIT).contains(&q.chars().count()) && !q.trim().is_empty()
}

fn is_query(v: &Value) -> bool {
    v.as_str().is_some_and(query_ok)
}

fn is_page(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|u| u.len() <= URL_LIMIT && web::key(u).is_some())
}

/// The tools the org's settings offer now.
pub(super) fn offered(s: &Settings) -> Vec<&'static Tool> {
    TOOLS.iter().filter(|t| (t.on)(s)).collect()
}

/// A call's args as the thread shows and keeps them: only the tool's
/// shown keys whose values pass their test (`kind`, an `art_` id, a
/// well-formed `query` and `url`), so never a title or `content`, or `{}`
/// when it is no object, no tool's or too long.
pub(super) fn shown(name: &str, args: &str) -> Value {
    let keys = TOOLS.iter().find(|t| t.name == name).map(|t| t.shown);
    match (keys, serde_json::from_str::<Value>(args)) {
        (Some(keys), Ok(Value::Object(mut o))) => {
            o.retain(|k, v| keys.iter().any(|(key, ok)| key == k && ok(v)));
            let v = Value::Object(o);
            if v.to_string().len() > RESULT_LIMIT {
                return json!({});
            }
            v
        }
        _ => json!({}),
    }
}

/// What a call answered: the result the model reads, and the event the
/// client gets when it made or changed an artifact.
pub(super) struct Outcome {
    pub result: Value,
    pub event: Option<Value>,
}

impl Outcome {
    pub fn error(code: &str) -> Outcome {
        Outcome {
            result: json!({ "error": code }),
            event: None,
        }
    }

    pub fn ok(&self) -> bool {
        self.result.get("error").is_none()
    }
}

/// One send's tools: who they act for, which are offered, the artifacts
/// made, tied to the reply once it is stored, and the pages this send met,
/// which alone `read_page` opens.
pub(super) struct Turn {
    st: St,
    member: String,
    offered: Vec<&'static Tool>,
    pub made: Vec<String>,
    met: HashSet<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Make {
    title: String,
    kind: String,
    language: Option<String>,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    id: String,
    content: String,
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Search {
    query: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    url: String,
}

fn title_ok(t: &str) -> Result<(), &'static str> {
    let n = t.chars().count();
    if n == 0 || n > TITLE_LIMIT || t.trim().is_empty() {
        return Err("bad_args");
    }
    Ok(())
}

fn content_ok(c: &str) -> Result<(), &'static str> {
    match c.len() {
        0 => Err("bad_args"),
        n if n > CONTENT_LIMIT => Err("too_large"),
        _ => Ok(()),
    }
}

fn language_ok(l: &str) -> bool {
    (1..=LANGUAGE_LIMIT).contains(&l.len())
        && l.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+#.-".contains(&b))
}

fn args<T: serde::de::DeserializeOwned>(o: Map<String, Value>) -> Result<T, &'static str> {
    serde_json::from_value(Value::Object(o)).map_err(|_| "bad_args")
}

fn failed(e: anyhow::Error) -> &'static str {
    eprintln!("fonsviva org: tool: {e:#}");
    "failed"
}

impl Turn {
    /// The tools `s` offers for a send of `text`, whose URLs it met.
    pub fn new(st: St, member: String, s: &Settings, text: &str) -> Turn {
        Turn {
            st,
            member,
            offered: offered(s),
            made: vec![],
            met: web::urls_in(text).into_iter().collect(),
        }
    }

    /// The definitions sent upstream.
    pub fn specs(&self) -> Value {
        self.offered
            .iter()
            .map(|t| serde_json::from_str::<Value>(t.spec).expect("tool spec"))
            .collect::<Vec<_>>()
            .into()
    }

    /// Runs a call: a tool not offered, args not of its shape and a tool
    /// that fails are each an `{"error"}` result.
    pub async fn call(&mut self, name: &str, raw: &str) -> Outcome {
        if !self.offered.iter().any(|t| t.name == name) {
            return Outcome::error("not_offered");
        }
        let Ok(Value::Object(o)) = serde_json::from_str::<Value>(raw) else {
            return Outcome::error("bad_args");
        };
        let done = match name {
            "make_artifact" => args(o).and_then(|a| self.make(a)),
            "update_artifact" => args(o).and_then(|a| self.update(a)),
            "web_search" => match args(o) {
                Ok(a) => self.search(a).await,
                Err(e) => Err(e),
            },
            "read_page" => match args(o) {
                Ok(a) => self.read(a).await,
                Err(e) => Err(e),
            },
            _ => Err("not_offered"),
        };
        done.unwrap_or_else(Outcome::error)
    }

    fn make(&mut self, a: Make) -> Result<Outcome, &'static str> {
        title_ok(&a.title)?;
        if !KINDS.contains(&a.kind.as_str()) {
            return Err("bad_args");
        }
        if let Some(l) = &a.language
            && (a.kind != "code" || !language_ok(l))
        {
            return Err("bad_args");
        }
        content_ok(&a.content)?;
        let mut store = self.st.store();
        if !store.settings().map_err(failed)?.keep_threads {
            return Ok(Outcome {
                result: json!({"id": null, "kept": false}),
                event: Some(json!({
                    "type": "artifact", "id": null, "title": a.title, "kind": a.kind,
                    "version": 1, "content": a.content,
                })),
            });
        }
        let made = store.make_artifact(
            &self.member,
            &a.title,
            &a.kind,
            a.language.as_deref(),
            &a.content,
        );
        match made.map_err(failed)? {
            Ok(id) => {
                self.made.push(id.clone());
                Ok(Outcome {
                    result: json!({"id": id, "version": 1}),
                    event: Some(json!({
                        "type": "artifact", "id": id, "title": a.title, "kind": a.kind, "version": 1,
                    })),
                })
            }
            Err(_) => Err("too_many_artifacts"),
        }
    }

    fn update(&mut self, a: Update) -> Result<Outcome, &'static str> {
        if let Some(t) = &a.title {
            title_ok(t)?;
        }
        content_ok(&a.content)?;
        let mut store = self.st.store();
        if !store.settings().map_err(failed)?.keep_threads {
            return Err("not_kept");
        }
        let updated = store.update_artifact(&self.member, &a.id, &a.content, a.title.as_deref());
        match updated.map_err(failed)? {
            Ok(row) => Ok(Outcome {
                result: json!({"id": row.id, "version": row.version}),
                event: Some(json!({
                    "type": "artifact", "id": row.id, "title": row.title, "kind": row.kind,
                    "version": row.version,
                })),
            }),
            Err(ArtifactRefused::TooManyVersions) => Err("too_many_versions"),
            Err(_) => Err("not_found"),
        }
    }

    /// The org's search service as the settings give it now, and whether
    /// it is local: `not_offered` once search is off.
    fn search_url(&self) -> Result<(String, bool), &'static str> {
        let s = self.st.store().settings().map_err(failed)?;
        s.search_url
            .map(|u| (u, s.search_local))
            .ok_or("not_offered")
    }

    async fn search(&mut self, a: Search) -> Result<Outcome, &'static str> {
        if !query_ok(&a.query) {
            return Err("bad_args");
        }
        let (base, local) = self.search_url()?;
        let client = self.st.sources.client(local);
        let results = web::search(client, &base, &a.query).await?;
        for r in &results {
            self.met.extend(r["url"].as_str().map(str::to_string));
        }
        Ok(Outcome {
            result: json!({ "results": results }),
            event: None,
        })
    }

    async fn read(&mut self, a: Read) -> Result<Outcome, &'static str> {
        self.search_url()?;
        let page = web::key(&a.url).filter(|u| self.met.contains(u));
        let page = page.ok_or("not_offered")?;
        let own: Vec<String> = self
            .st
            .store()
            .sources()
            .map_err(failed)?
            .iter()
            .filter_map(|s| web::host_of(&s.base_url))
            .collect();
        let u = url::Url::parse(&page).map_err(|_| "bad_args")?;
        Ok(Outcome {
            result: web::read(u, &own).await?,
            event: None,
        })
    }
}
