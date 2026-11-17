//! Tokens, scopes, app keys and request origin checks (docs/API.md "Auth").

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use fonsviva_core::util::{hex, rfc3339, unix_now};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Owner,
    Control,
    Watch,
}

/// What a request does, for the scope check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Read,
    Pull,
    Run,
    Stop,
    Wire,
    Bench,
    CancelJob,
    Keys,
    /// Local Studio's per-plugin keys: owner or control.
    PluginKeys,
    MintWatch,
    PodStop,
    CloudSpend,
    Shutdown,
}

pub fn allowed(scope: Scope, action: Action) -> bool {
    use Action::*;
    match scope {
        Scope::Owner => true,
        Scope::Control => !matches!(action, Keys | CloudSpend | Shutdown),
        Scope::Watch => matches!(action, Read | Run | Stop | PodStop),
    }
}

/// Random hex from the OS.
pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    getrandom::fill(&mut b).expect("OS random source");
    hex(&b)
}

pub fn sha256_hex(s: &str) -> String {
    hex(&Sha256::digest(s.as_bytes()))
}

/// HMAC-SHA256 (RFC 2104), hex.
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |b: u8| k.iter().map(|x| x ^ b).collect::<Vec<u8>>();
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(msg)
        .finalize();
    hex(&Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize())
}

/// Compares secrets without leaking where they differ.
pub fn ct_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Writes a file only the owner can read (0600 on Unix).
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        use std::io::Write;
        let mut f = o
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Reads a secret file, creating it with `make` the first time.
pub fn read_or_create(path: &Path, make: impl FnOnce() -> String) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => {
            let v = make();
            write_private(path, v.as_bytes())?;
            Ok(v)
        }
    }
}

/// The profile an app's key holds. Fixed in code; the caller never picks it.
pub fn profile_for(app: &str) -> Option<&'static str> {
    match app {
        "untocaesar" => Some("untocaesar"),
        "corunis" => Some("corunis"),
        "gantrel" => Some("gantrel"),
        // The desktop app, the CLI and wired agents.
        "desktop" | "cli" | "pi" | "t3code" | "openai" | "eukhe" => Some("personal"),
        _ => None,
    }
}

/// App name prefix of Local Studio plugin keys.
pub const PLUGIN_APP: &str = "local-studio-plugin:";

/// An app key as stored: the secret is kept only as a hash.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyRecord {
    pub id: String,
    pub app: String,
    pub profile: String,
    /// First characters of the secret, so a person can tell keys apart.
    pub prefix: String,
    pub created_at: String,
    pub sha256: String,
}

impl KeyRecord {
    /// Plugin keys never reach cloud models and have no cloud switch.
    pub fn is_plugin(&self) -> bool {
        self.app.starts_with(PLUGIN_APP)
    }
}

/// A watch token as stored.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenRecord {
    pub id: String,
    pub scope: Scope,
    pub label: String,
    pub created_at: String,
    pub sha256: String,
}

/// App keys in `shared/keys.json`, shared by stable and nightly. The format
/// only ever gains fields.
pub struct Keys {
    path: PathBuf,
    pub list: Vec<KeyRecord>,
}

impl Keys {
    pub fn load(path: PathBuf) -> Result<Keys> {
        let list = match std::fs::read_to_string(&path) {
            Ok(s) => {
                serde_json::from_str(&s).with_context(|| format!("parse {}", path.display()))?
            }
            Err(_) => vec![],
        };
        Ok(Keys { path, list })
    }

    fn save(&self) -> Result<()> {
        write_private(&self.path, &serde_json::to_vec_pretty(&self.list)?)
    }

    /// Creates a key for `app`; the secret is returned once and never stored.
    pub fn create(&mut self, app: &str) -> Result<(KeyRecord, String)> {
        let Some(profile) = profile_for(app) else {
            bail!(
                "unknown app {app:?}; known apps: desktop, cli, pi, t3code, openai, eukhe, untocaesar, corunis, gantrel"
            );
        };
        self.insert(app.into(), profile.into())
    }

    /// Creates the key for a Local Studio plugin. One key per plugin: an
    /// existing one is revoked, so a lost secret can be replaced.
    pub fn create_plugin(&mut self, plugin: &str) -> Result<(KeyRecord, String)> {
        let ok = !plugin.is_empty()
            && plugin.len() <= 64
            && plugin.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.')
            });
        if !ok {
            bail!(
                "plugin id {plugin:?} must be 1 to 64 lowercase letters, digits, '-', '_' or '.'"
            );
        }
        let app = format!("{PLUGIN_APP}{plugin}");
        self.list.retain(|k| k.app != app);
        let profile = match plugin {
            "untocaesar" | "corunis" | "gantrel" => plugin,
            _ => "plugin",
        };
        self.insert(app, profile.into())
    }

    fn insert(&mut self, app: String, profile: String) -> Result<(KeyRecord, String)> {
        let secret = format!("fv-{}", random_hex(24));
        let rec = KeyRecord {
            id: format!("key_{}", random_hex(6)),
            app,
            profile,
            prefix: secret[..9].to_string(),
            created_at: rfc3339(unix_now()),
            sha256: sha256_hex(&secret),
        };
        self.list.push(rec.clone());
        self.save()?;
        Ok((rec, secret))
    }

    pub fn revoke(&mut self, id: &str) -> Result<bool> {
        let n = self.list.len();
        self.list.retain(|k| k.id != id);
        if self.list.len() == n {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    pub fn check(&self, secret: &str) -> Option<&KeyRecord> {
        let h = sha256_hex(secret);
        self.list.iter().find(|k| ct_eq(&k.sha256, &h))
    }
}

pub struct Tokens {
    path: PathBuf,
    pub list: Vec<TokenRecord>,
}

impl Tokens {
    pub fn load(path: PathBuf) -> Result<Tokens> {
        let list = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)?,
            Err(_) => vec![],
        };
        Ok(Tokens { path, list })
    }

    pub fn mint_watch(&mut self, label: &str) -> Result<(TokenRecord, String)> {
        let secret = random_hex(32);
        let rec = TokenRecord {
            id: format!("tok_{}", random_hex(6)),
            scope: Scope::Watch,
            label: label.into(),
            created_at: rfc3339(unix_now()),
            sha256: sha256_hex(&secret),
        };
        self.list.push(rec.clone());
        write_private(&self.path, &serde_json::to_vec_pretty(&self.list)?)?;
        Ok((rec, secret))
    }

    pub fn revoke(&mut self, id: &str) -> Result<bool> {
        let n = self.list.len();
        self.list.retain(|t| t.id != id);
        if self.list.len() == n {
            return Ok(false);
        }
        write_private(&self.path, &serde_json::to_vec_pretty(&self.list)?)?;
        Ok(true)
    }

    pub fn check(&self, secret: &str) -> Option<Scope> {
        let h = sha256_hex(secret);
        self.list
            .iter()
            .find(|t| ct_eq(&t.sha256, &h))
            .map(|t| t.scope)
    }
}

/// `Host` must name this daemon on loopback: blocks DNS rebinding.
pub fn host_ok(host: Option<&str>, port: u16) -> bool {
    let Some(h) = host else { return false };
    let h = h.trim().to_ascii_lowercase();
    h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}")
}

/// Browser origins allowed on `/api/v1/*`: only the desktop app's own.
pub fn origin_ok_for_api(origin: Option<&str>, dev: bool) -> bool {
    match origin.map(str::trim) {
        None => true,
        Some("tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost") => true,
        // The app's dev server; stable and nightly never accept it.
        Some("http://localhost:1430") => dev,
        Some(_) => false,
    }
}

pub fn bearer(header: Option<&str>) -> Option<&str> {
    header?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2.
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: key longer than a block.
        assert_eq!(
            hmac_sha256_hex(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn scopes() {
        use Action::*;
        assert!(allowed(Scope::Owner, CloudSpend));
        assert!(
            allowed(Scope::Control, Pull)
                && allowed(Scope::Control, PodStop)
                && allowed(Scope::Control, MintWatch)
        );
        assert!(!allowed(Scope::Control, Keys) && !allowed(Scope::Control, CloudSpend));
        assert!(
            allowed(Scope::Watch, Run)
                && allowed(Scope::Watch, Stop)
                && allowed(Scope::Watch, PodStop)
        );
        assert!(allowed(Scope::Control, PluginKeys) && allowed(Scope::Owner, PluginKeys));
        for a in [
            Pull, Wire, Keys, PluginKeys, MintWatch, CloudSpend, Bench, CancelJob, Shutdown,
        ] {
            assert!(!allowed(Scope::Watch, a), "{a:?}");
        }
    }

    #[test]
    fn app_profiles_are_fixed() {
        assert_eq!(profile_for("untocaesar"), Some("untocaesar"));
        assert_eq!(profile_for("corunis"), Some("corunis"));
        assert_eq!(profile_for("gantrel"), Some("gantrel"));
        assert_eq!(profile_for("desktop"), Some("personal"));
        assert_eq!(profile_for("pi"), Some("personal"));
        assert_eq!(profile_for("anything"), None);
    }

    #[test]
    fn keys_store_only_hashes() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("shared/keys.json");
        let mut k = Keys::load(p.clone()).unwrap();
        let (rec, secret) = k.create("untocaesar").unwrap();
        assert_eq!(rec.profile, "untocaesar");
        let on_disk = std::fs::read_to_string(&p).unwrap();
        assert!(!on_disk.contains(&secret));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let k = Keys::load(p.clone()).unwrap();
        assert_eq!(k.check(&secret).unwrap().id, rec.id);
        assert!(k.check("fv-wrong").is_none());
        let mut k = k;
        assert!(k.create("stranger").is_err());
        assert!(k.revoke(&rec.id).unwrap());
        assert!(k.check(&secret).is_none());
    }

    #[test]
    fn host_and_origin() {
        assert!(host_ok(Some("127.0.0.1:7342"), 7342));
        assert!(host_ok(Some("localhost:7342"), 7342));
        assert!(!host_ok(Some("evil.example:7342"), 7342));
        assert!(!host_ok(Some("127.0.0.1:7341"), 7342));
        assert!(!host_ok(None, 7342));
        assert!(origin_ok_for_api(None, false));
        assert!(origin_ok_for_api(Some("tauri://localhost"), false));
        assert!(origin_ok_for_api(Some("http://tauri.localhost"), false));
        assert!(!origin_ok_for_api(Some("https://evil.example"), true));
        assert!(!origin_ok_for_api(Some("null"), true));
        assert!(origin_ok_for_api(Some("http://localhost:1430"), true));
        assert!(!origin_ok_for_api(Some("http://localhost:1430"), false));
    }

    #[test]
    fn one_plugin_key_per_plugin_with_a_fixed_profile() {
        let d = tempfile::tempdir().unwrap();
        let mut k = Keys::load(d.path().join("keys.json")).unwrap();
        let (first, s1) = k.create_plugin("notes").unwrap();
        assert_eq!(first.app, "local-studio-plugin:notes");
        assert_eq!(first.profile, "plugin");
        assert!(first.is_plugin());
        let (second, s2) = k.create_plugin("notes").unwrap();
        assert!(k.check(&s1).is_none(), "re-creating revokes the old key");
        assert_eq!(k.check(&s2).unwrap().id, second.id);
        assert_eq!(k.create_plugin("corunis").unwrap().0.profile, "corunis");
        assert_eq!(k.create_plugin("gantrel").unwrap().0.profile, "gantrel");
        assert!(k.create_plugin("Bad Id").is_err() && k.create_plugin("").is_err());
        assert!(!k.create("desktop").unwrap().0.is_plugin());
    }
}
