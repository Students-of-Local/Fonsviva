//! Seals source keys with ChaCha20-Poly1305 under `secrets.key`, a 32-byte
//! key that sits next to org.db and never inside it.

use anyhow::{Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use std::path::{Path, PathBuf};

pub const FILE: &str = "secrets.key";
const NONCE: usize = 12;

fn path(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// The key in `dir`, or `None` when it is missing or not 32 bytes.
pub fn read(dir: &Path) -> Option<[u8; 32]> {
    std::fs::read(path(dir)).ok()?.try_into().ok()
}

pub fn exists(dir: &Path) -> bool {
    path(dir).exists()
}

/// The key in `dir`, made at mode 0600 when there is none yet. A new key is
/// written whole to a temp file and linked into place, so no reader ever
/// sees part of one and a key already there is never replaced.
pub fn ensure(dir: &Path) -> Result<[u8; 32]> {
    let p = path(dir);
    if p.exists() {
        return read(dir).with_context(|| format!("{} is not a 32-byte key", p.display()));
    }
    let tmp = dir.join(format!(".{FILE}.{}", crate::auth::random_hex(8)));
    let mut new = std::fs::OpenOptions::new();
    new.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        new.mode(0o600);
    }
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).expect("OS random source");
    let written = new
        .open(&tmp)
        .and_then(|mut f| std::io::Write::write_all(&mut f, &k).and_then(|()| f.sync_all()))
        .with_context(|| format!("write {}", tmp.display()));
    let linked = written.and_then(|()| match std::fs::hard_link(&tmp, &p) {
        Ok(()) => Ok(Some(k)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(e).with_context(|| format!("link {}", p.display())),
    });
    let _ = std::fs::remove_file(&tmp);
    match linked? {
        Some(k) => Ok(k),
        None => read(dir).with_context(|| format!("{} is not a 32-byte key", p.display())),
    }
}

/// A random nonce followed by the sealed text, bound to `id`.
pub fn seal(key: &[u8; 32], id: &str, plain: &str) -> Vec<u8> {
    let mut nonce = [0u8; NONCE];
    getrandom::fill(&mut nonce).expect("OS random source");
    let sealed = ChaCha20Poly1305::new(key.into())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain.as_bytes(),
                aad: id.as_bytes(),
            },
        )
        .expect("sealing a short key");
    [nonce.as_slice(), &sealed].concat()
}

/// The text `seal` sealed for `id`, or `None` for another key or id.
pub fn open(key: &[u8; 32], id: &str, sealed: &[u8]) -> Option<String> {
    if sealed.len() < NONCE {
        return None;
    }
    let (nonce, text) = sealed.split_at(NONCE);
    let plain = ChaCha20Poly1305::new(key.into())
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: text,
                aad: id.as_bytes(),
            },
        )
        .ok()?;
    String::from_utf8(plain).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn racing_first_uses_agree_on_one_whole_key() {
        for _ in 0..40 {
            let d = tempfile::tempdir().unwrap();
            let keys: Vec<_> = std::thread::scope(|sc| {
                let hs: Vec<_> = (0..8).map(|_| sc.spawn(|| ensure(d.path()))).collect();
                hs.into_iter().map(|h| h.join().unwrap().unwrap()).collect()
            });
            assert!(keys.iter().all(|k| *k == keys[0]));
            assert_eq!(read(d.path()), Some(keys[0]));
            let left: Vec<_> = std::fs::read_dir(d.path()).unwrap().collect();
            assert_eq!(left.len(), 1, "temp files left behind");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(path(d.path()))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600);
            }
        }
    }
}
