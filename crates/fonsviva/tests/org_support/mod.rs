//! Runs the real `fonsviva org` commands on scratch dirs, and `org serve` on
//! a free loopback port, with a plain HTTP/1.1 client so every header is the
//! test's own. Nothing here touches the user's state.

#![allow(dead_code)]

pub mod stub;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

pub const URL: &str = "https://org.example";

pub fn fonsviva(scratch: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fonsviva"));
    c.env("HOME", scratch.join("home"))
        .env("USERPROFILE", scratch.join("home"))
        .env("FONSVIVA_HOME", scratch.join("fv"))
        .env("FONSVIVA_PORT", "0")
        .env("XDG_CACHE_HOME", scratch.join("cache"))
        .env("XDG_CONFIG_HOME", scratch.join("config"))
        .env("XDG_DATA_HOME", scratch.join("share"))
        .env("LOCALAPPDATA", scratch.join("local"))
        .env("APPDATA", scratch.join("roaming"))
        .stdin(Stdio::null());
    c
}

pub fn init(scratch: &Path, data: &Path, url: &str) -> Output {
    fonsviva(scratch)
        .args(["org", "init", "--url", url, "--data"])
        .arg(data)
        .output()
        .unwrap()
}

/// The token at the end of a join link.
pub fn token_of(link: &str) -> String {
    link.trim().rsplit('/').next().unwrap().to_string()
}

pub struct Org {
    pub scratch: tempfile::TempDir,
    pub data: PathBuf,
    child: Child,
    pub port: u16,
    pub admin_link: String,
    pub stderr: std::sync::Arc<std::sync::Mutex<String>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Org {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn start() -> Org {
    start_on(&["--listen", "127.0.0.1:0"])
}

/// Starts `org serve` with these listen flags.
pub fn start_on(listen: &[&str]) -> Org {
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("org");
    let out = init(scratch.path(), &data, URL);
    assert!(out.status.success(), "init: {out:?}");
    let admin_link = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let mut child = fonsviva(scratch.path())
        .args(["org", "serve"])
        .args(listen)
        .arg("--data")
        .arg(&data)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap());
    let mut first = String::new();
    lines.read_line(&mut first).unwrap();
    let port = first
        .trim()
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("serve said {first:?}"));
    let stderr = std::sync::Arc::new(std::sync::Mutex::new(first));
    let sink = stderr.clone();
    let reader = std::thread::spawn(move || {
        for line in lines.lines() {
            let line = line.unwrap();
            let mut s = sink.lock().unwrap();
            s.push_str(&line);
            s.push('\n');
        }
    });
    Org {
        scratch,
        data,
        child,
        port,
        admin_link,
        stderr,
        reader: Some(reader),
    }
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|_| panic!("not JSON: {}", self.body))
    }

    /// The session cookie this answer set, as `fv_session=<token>`.
    pub fn session(&self) -> String {
        let c = self.header("set-cookie").expect("set-cookie");
        c.split(';').next().unwrap().to_string()
    }
}

pub struct Req<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub host: Option<String>,
    pub headers: Vec<(&'a str, String)>,
    pub body: Option<String>,
}

impl Org {
    /// Stops the server and returns everything it wrote to stderr.
    pub fn stop(&mut self) -> String {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
        self.stderr.lock().unwrap().clone()
    }

    pub fn host(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub fn get(&self, path: &str, cookie: Option<&str>) -> Resp {
        let mut r = Req::new("GET", path, self);
        if let Some(c) = cookie {
            r.headers.push(("Cookie", c.into()));
        }
        self.send(r)
    }

    /// A POST or PATCH the way the web app sends it.
    pub fn call(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: serde_json::Value,
    ) -> Resp {
        let mut r = Req::new(method, path, self);
        r.headers.push(("X-Fonsviva-Request", "1".into()));
        if let Some(c) = cookie {
            r.headers.push(("Cookie", c.into()));
        }
        r.body = Some(body.to_string());
        self.send(r)
    }

    /// A join sent with extra headers, such as a tunnel's client address.
    pub fn join_as(&self, token: &str, headers: &[(&'static str, &str)]) -> Resp {
        let mut r = Req::new("POST", "/api/org/v1/join", self);
        r.headers.push(("X-Fonsviva-Request", "1".into()));
        for (k, v) in headers {
            r.headers.push((k, v.to_string()));
        }
        r.body = Some(serde_json::json!({"token": token, "name": "Mallory"}).to_string());
        self.send(r)
    }

    pub fn join(&self, token: &str, name: &str) -> Resp {
        self.call(
            "POST",
            "/api/org/v1/join",
            None,
            serde_json::json!({"token": token, "name": name}),
        )
    }

    /// Sends a request and returns the open connection, so a test can read
    /// the answer as it arrives.
    pub fn open(&self, r: Req) -> TcpStream {
        let mut s = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let mut head = format!("{} {} HTTP/1.1\r\nConnection: close\r\n", r.method, r.path);
        if let Some(h) = &r.host {
            head.push_str(&format!("Host: {h}\r\n"));
        }
        for (k, v) in &r.headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        let body = r.body.unwrap_or_default();
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        s.write_all(head.as_bytes()).unwrap();
        s
    }

    pub fn send(&self, r: Req) -> Resp {
        let mut s = self.open(r);
        let mut raw = String::new();
        s.read_to_string(&mut raw).unwrap();
        let (head, body) = raw.split_once("\r\n\r\n").unwrap();
        let mut lines = head.lines();
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let mut r = Resp {
            status,
            headers,
            body: body.to_string(),
        };
        if r.header("transfer-encoding") == Some("chunked") {
            r.body = dechunk(&r.body);
        }
        r
    }

    /// The bytes of org.db and its write-ahead log, where recent writes sit.
    pub fn db_bytes(&self) -> Vec<u8> {
        let mut all = vec![];
        for f in ["org.db", "org.db-wal"] {
            if let Ok(b) = std::fs::read(self.data.join(f)) {
                all.extend(b);
            }
        }
        all
    }

    pub fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.data.join("org.db")).unwrap()
    }
}

impl<'a> Req<'a> {
    pub fn new(method: &'a str, path: &'a str, org: &Org) -> Req<'a> {
        Req {
            method,
            path,
            host: Some(org.host()),
            headers: vec![],
            body: None,
        }
    }
}

fn dechunk(mut s: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = s.split_once("\r\n") {
        let n = usize::from_str_radix(size.trim(), 16).unwrap();
        if n == 0 {
            break;
        }
        out.push_str(&rest[..n]);
        s = &rest[n + 2..];
    }
    out
}

/// True when `needle` appears in `hay`.
pub fn holds(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

/// Joins with the admin link and returns the admin's cookie and id.
pub fn admin(org: &Org) -> (String, String) {
    let r = org.join(&token_of(&org.admin_link), "Ada");
    assert_eq!(r.status, 200);
    (r.session(), r.json()["id"].as_str().unwrap().to_string())
}

/// A new member with `role`, through an invite: their cookie and id.
pub fn member(org: &Org, admin: &str, role: &str, name: &str) -> (String, String) {
    let r = org.call(
        "POST",
        "/api/org/v1/invites",
        Some(admin),
        serde_json::json!({"role": role, "expiresHours": 1, "uses": 1}),
    );
    assert_eq!(r.status, 200, "{}", r.body);
    let link = r.json()["link"].as_str().unwrap().to_string();
    let j = org.join(&token_of(&link), name);
    assert_eq!(j.status, 200);
    (j.session(), j.json()["id"].as_str().unwrap().to_string())
}

/// Adds a source and returns its JSON answer.
pub fn add_source(org: &Org, admin: &str, name: &str, base: &str, key: Option<&str>) -> Resp {
    let mut b = serde_json::json!({"name": name, "baseUrl": base});
    if let Some(k) = key {
        b["apiKey"] = k.into();
    }
    org.call("POST", "/api/org/v1/sources", Some(admin), b)
}

/// The key the stub source is added with.
pub const SOURCE_KEY: &str = "sk-stub-secret-4f1c9a";

/// An org with an admin, a user and the stub added as `stub` with
/// `SOURCE_KEY`: the org, the stub, the admin's and the user's cookies.
pub fn ready() -> (Org, stub::Stub, String, String) {
    let org = start();
    let stub = stub::Stub::start();
    let (a, _) = admin(&org);
    let r = add_source(&org, &a, "stub", &stub.base(), Some(SOURCE_KEY));
    assert_eq!(r.status, 200, "{}", r.body);
    let (u, _) = member(&org, &a, "user", "Bo");
    (org, stub, a, u)
}
