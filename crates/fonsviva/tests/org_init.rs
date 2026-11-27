//! `fonsviva org init` and `fonsviva org admin-link` on scratch dirs.

mod org_support;

use org_support::{fonsviva, init, token_of};
use std::path::Path;

fn admin_link(scratch: &Path, data: &Path) -> std::process::Output {
    fonsviva(scratch)
        .args(["org", "admin-link", "--data"])
        .arg(data)
        .output()
        .unwrap()
}

#[test]
fn init_refuses_a_dir_that_holds_org_db() {
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("org");
    assert!(
        init(scratch.path(), &data, "https://org.example")
            .status
            .success()
    );
    let db = data.join("org.db");
    let before = std::fs::read(&db).unwrap();
    let mtime = std::fs::metadata(&db).unwrap().modified().unwrap();
    let out = init(scratch.path(), &data, "https://org.example");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty());
    assert_eq!(std::fs::read(&db).unwrap(), before);
    assert_eq!(std::fs::metadata(&db).unwrap().modified().unwrap(), mtime);
}

#[test]
fn init_refuses_a_url_with_a_path() {
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("org");
    let out = init(scratch.path(), &data, "https://org.example/fonsviva");
    assert_eq!(out.status.code(), Some(1));
    assert!(!data.join("org.db").exists());
}

#[cfg(unix)]
#[test]
fn init_makes_the_store_private() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("org");
    let out = init(scratch.path(), &data, "http://org.example:8080");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.starts_with("http://org.example:8080/join/"));
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&data.join("org.db")), 0o600);
    assert_eq!(mode(&data.join("secrets.key")), 0o600);
    assert_eq!(std::fs::read(data.join("secrets.key")).unwrap().len(), 32);
    let db = rusqlite::Connection::open(data.join("org.db")).unwrap();
    let v: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(v, 9);
    let wal: String = db
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(wal, "wal");
}

#[test]
fn admin_link_revokes_unused_older_links() {
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("org");
    let first = init(scratch.path(), &data, "https://org.example/");
    assert!(first.status.success());
    let first = token_of(&String::from_utf8(first.stdout).unwrap());
    let out = admin_link(scratch.path(), &data);
    assert!(out.status.success(), "{out:?}");
    let link = String::from_utf8(out.stdout).unwrap();
    assert_eq!(link.lines().count(), 1);
    assert!(link.starts_with("https://org.example/join/"));
    let second = token_of(&link);
    assert_ne!(first, second);
    let db = rusqlite::Connection::open(data.join("org.db")).unwrap();
    let rows: Vec<(String, bool, i64, i64)> = db
        .prepare("SELECT role, revoked, uses, expires_at - created_at FROM invites ORDER BY rowid")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let day = 24 * 3600;
    assert_eq!(
        rows,
        [
            ("admin".to_string(), true, 1, day),
            ("admin".to_string(), false, 1, day)
        ]
    );
    let raw = String::from_utf8_lossy(&std::fs::read(data.join("org.db")).unwrap()).to_string()
        + &String::from_utf8_lossy(&std::fs::read(data.join("org.db-wal")).unwrap_or_default());
    assert!(
        !raw.contains(&first) && !raw.contains(&second),
        "only hashes are stored"
    );
    let actions: Vec<String> = db
        .prepare("SELECT action FROM audit ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(actions, ["admin_link.create", "admin_link.create"]);
}

#[test]
fn admin_link_needs_an_org() {
    let scratch = tempfile::tempdir().unwrap();
    let out = admin_link(scratch.path(), &scratch.path().join("none"));
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(!scratch.path().join("none/org.db").exists());
}

#[test]
fn a_key_that_init_cannot_use_leaves_no_store() {
    let scratch = tempfile::tempdir().unwrap();
    let data = scratch.path().join("org");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("secrets.key"), b"short").unwrap();
    let out = init(scratch.path(), &data, "https://org.example");
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is not a 32-byte key"),
        "{out:?}"
    );
    assert!(!data.join("org.db").exists(), "no store without its key");
    std::fs::remove_file(data.join("secrets.key")).unwrap();
    let out = init(scratch.path(), &data, "https://org.example");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read(data.join("secrets.key")).unwrap().len(), 32);
}
