//! `fonsviva org`: the org server. Members, roles and sign-in links on one
//! SQLite store (docs/ARCHITECTURE.md).

mod chat;
mod costs;
mod http;
mod limits;
mod seal;
mod send;
mod sources;
mod store;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use store::Store;

/// A refusal the CLI exits 2 on, so scripts can tell it from a failure.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn db_path(dir: &Path) -> PathBuf {
    dir.join("org.db")
}

/// The org URL: http or https, a host, and no path, query or user.
fn parse_url(s: &str) -> Result<url::Url> {
    let u =
        url::Url::parse(s.trim_end_matches('/')).with_context(|| format!("{s} is not a URL"))?;
    let ok = matches!(u.scheme(), "http" | "https")
        && u.host_str().is_some()
        && u.path() == "/"
        && u.query().is_none()
        && u.fragment().is_none()
        && u.username().is_empty()
        && u.password().is_none();
    if !ok {
        anyhow::bail!(
            "{s} must be an http or https address with no path, such as https://fonsviva.example.org"
        );
    }
    Ok(u)
}

fn base(u: &url::Url) -> String {
    u.as_str().trim_end_matches('/').to_string()
}

fn link(url: &str, token: &str) -> String {
    format!("{url}/join/{token}")
}

pub fn init(dir: &Path, url: &str) -> Result<()> {
    let url = base(&parse_url(url)?);
    let db = db_path(dir);
    if db.exists() {
        return Err(Refused(format!("{} already holds an org.db.", dir.display())).into());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut new = std::fs::OpenOptions::new();
    new.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        new.mode(0o600);
    }
    seal::ensure(dir)?;
    match new.open(&db) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(Refused(format!("{} already holds an org.db.", dir.display())).into());
        }
        Err(e) => return Err(e).with_context(|| format!("create {}", db.display())),
    }
    let mut store = Store::create(&db, &url)?;
    let token = store.new_admin_link("cli")?;
    println!("{}", link(&url, &token));
    Ok(())
}

pub fn admin_link(dir: &Path) -> Result<()> {
    let mut store = Store::open(&db_path(dir))?;
    let url = store.url()?;
    let token = store.new_admin_link("cli")?;
    println!("{}", link(&url, &token));
    Ok(())
}

pub async fn serve(dir: &Path, listen: SocketAddr, public: bool, trust_cf: bool) -> Result<()> {
    if trust_cf && public {
        return Err(Refused(
            "--trust-cf is for a loopback listener behind Cloudflare; on a public address anyone can send CF-Connecting-IP.".into(),
        )
        .into());
    }
    if !listen.ip().is_loopback() && !public {
        return Err(Refused(format!(
            "{listen} is not a loopback address. A tunnel or reverse proxy should front the org server; pass --public to listen there anyway."
        ))
        .into());
    }
    let store = Store::open(&db_path(dir))?;
    let url = parse_url(&store.url()?)?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listen on {listen}"))?;
    let addr = listener.local_addr()?;
    eprintln!("Fonsviva org server for {} on http://{addr}", base(&url));
    let st = Arc::new(http::OrgState::new(store, &url, addr, trust_cf));
    http::run(listener, st).await
}
