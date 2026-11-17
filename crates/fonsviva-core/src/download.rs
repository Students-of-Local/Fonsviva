//! Resumable, checksum-verified downloads.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncWriteExt;

use crate::util::hex;

#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    /// Bytes fetched over the network in this run (resume skips the rest).
    pub fetched: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst)
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("fonsviva/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .expect("http client")
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".part");
    PathBuf::from(p)
}

/// Downloads `url` to `dest`, resuming from `dest.part`, and checks the size
/// and sha256 before the file takes its final name. Returns bytes fetched now.
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    sha256: &str,
    size: u64,
    cancel: &Cancel,
    mut on_progress: impl FnMut(&Progress),
) -> Result<u64> {
    if let Some(dir) = dest.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let part = part_path(dest);
    let mut hasher = Sha256::new();
    let mut have = 0u64;
    if let Ok(mut f) = tokio::fs::File::open(&part).await {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            have += n as u64;
        }
    }
    if size > 0 && have > size {
        tokio::fs::remove_file(&part).await?;
        hasher = Sha256::new();
        have = 0;
    }
    let mut prog = Progress {
        done: have,
        total: size,
        fetched: 0,
    };
    if size == 0 || have < size {
        let mut req = client.get(url);
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("download {url}"))?;
        let status = resp.status();
        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            bail!(
                "the server refused to resume; delete {} and retry",
                part.display()
            );
        }
        if !status.is_success() {
            bail!("download failed: HTTP {status} from {url}");
        }
        if have > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            // Server ignored the range: start over.
            hasher = Sha256::new();
            have = 0;
            prog.done = 0;
            tokio::fs::write(&part, b"").await?;
        }
        if prog.total == 0 {
            prog.total = resp.content_length().map(|l| l + have).unwrap_or(0);
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part)
            .await?;
        let mut stream = resp.bytes_stream();
        let mut last = std::time::Instant::now();
        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                file.flush().await?;
                bail!("cancelled");
            }
            let chunk = chunk.context("download interrupted; run it again to resume")?;
            file.write_all(&chunk).await?;
            hasher.update(&chunk);
            prog.done += chunk.len() as u64;
            prog.fetched += chunk.len() as u64;
            if last.elapsed().as_millis() >= 250 {
                last = std::time::Instant::now();
                on_progress(&prog);
            }
        }
        file.flush().await?;
        file.sync_all().await?;
    }
    on_progress(&prog);
    if size > 0 && prog.done != size {
        bail!(
            "download stopped at {} of {} bytes; run it again to resume",
            prog.done,
            size
        );
    }
    let got = hex(&hasher.finalize());
    if !sha256.is_empty() && !got.eq_ignore_ascii_case(sha256) {
        let _ = tokio::fs::remove_file(&part).await;
        bail!(
            "checksum mismatch for {url}: expected {sha256}, got {got}; the partial file was removed"
        );
    }
    tokio::fs::rename(&part, dest).await?;
    Ok(prog.fetched)
}

/// sha256 of a whole file, in a blocking task.
pub async fn sha256_file(path: &Path) -> Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut f = std::fs::File::open(&path)?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = std::io::Read::read(&mut f, &mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(hex(&h.finalize()))
    })
    .await?
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A tiny HTTP/1.1 file server that honors `Range` and can cut the body
    /// short after `cut` bytes, to test resume.
    pub async fn serve_bytes(body: Vec<u8>, cut: Option<usize>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let body = Arc::new(body);
        let first = Arc::new(AtomicBool::new(true));
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let body = body.clone();
                let first = first.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = s.read(&mut buf).await.unwrap();
                    let req = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                    let start = req
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .and_then(|r| {
                            r.trim_end_matches('-')
                                .trim_end_matches("-\r")
                                .parse::<usize>()
                                .ok()
                        })
                        .unwrap_or(0);
                    let slice = &body[start..];
                    let status = if start > 0 {
                        "206 Partial Content"
                    } else {
                        "200 OK"
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        slice.len()
                    );
                    s.write_all(head.as_bytes()).await.unwrap();
                    let send = match cut {
                        Some(c) if first.swap(false, Ordering::SeqCst) => &slice[..c],
                        _ => slice,
                    };
                    let _ = s.write_all(send).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        format!("http://{addr}/file")
    }

    #[tokio::test]
    async fn resumes_and_verifies() {
        let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let sha = hex(&Sha256::digest(&body));
        let url = serve_bytes(body.clone(), Some(70_000)).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("m.gguf");
        let c = http_client();
        let first = fetch(
            &c,
            &url,
            &dest,
            &sha,
            body.len() as u64,
            &Cancel::default(),
            |_| {},
        )
        .await;
        assert!(first.is_err(), "first attempt is cut short");
        assert_eq!(std::fs::metadata(part_path(&dest)).unwrap().len(), 70_000);
        let fetched = fetch(
            &c,
            &url,
            &dest,
            &sha,
            body.len() as u64,
            &Cancel::default(),
            |_| {},
        )
        .await
        .unwrap();
        assert_eq!(fetched, 130_000, "only the rest is fetched");
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        assert!(!part_path(&dest).exists());
    }

    #[tokio::test]
    async fn rejects_bad_checksum() {
        let url = serve_bytes(vec![1, 2, 3, 4], None).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("m.gguf");
        let err = fetch(
            &http_client(),
            &url,
            &dest,
            &"0".repeat(64),
            4,
            &Cancel::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
        assert!(!dest.exists() && !part_path(&dest).exists());
    }
}
