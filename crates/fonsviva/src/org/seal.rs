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

/// The key in `dir`, made at mode 0600 when there is none yet.
pub fn ensure(dir: &Path) -> Result<[u8; 32]> {
    let p = path(dir);
    let mut new = std::fs::OpenOptions::new();
    new.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        new.mode(0o600);
    }
    match new.open(&p) {
        Ok(mut f) => {
            let mut k = [0u8; 32];
            getrandom::fill(&mut k).expect("OS random source");
            std::io::Write::write_all(&mut f, &k)
                .and_then(|()| f.sync_all())
                .with_context(|| format!("write {}", p.display()))?;
            Ok(k)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            read(dir).with_context(|| format!("{} is not a 32-byte key", p.display()))
        }
        Err(e) => Err(e).with_context(|| format!("create {}", p.display())),
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
