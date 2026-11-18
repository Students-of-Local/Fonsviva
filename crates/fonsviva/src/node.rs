//! `fonsviva node`: pair with another machine and carry engine traffic to it
//! over one link, through the node channel.

use std::net::{SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use fonsviva_core::channel::{self, Key, Link};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const PAIR_PORT: u16 = 7350;
const PAIR_WAIT: Duration = Duration::from_secs(600);

/// This machine's node name: its host name, lowercased, with other characters
/// turned into dashes.
pub fn this_name() -> String {
    let host = host_name().unwrap_or_else(|| "node".into());
    let name: String = host
        .split('.')
        .next()
        .unwrap_or("node")
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .take(64)
        .collect();
    if channel::valid_name(&name) {
        name
    } else {
        "node".into()
    }
}

fn host_name() -> Option<String> {
    let out = std::process::Command::new("hostname").output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

fn name_or_default(name: Option<String>) -> Result<String> {
    let name = name.unwrap_or_else(this_name);
    if !channel::valid_name(&name) {
        bail!("node name {name:?} must be lowercase letters, digits and dashes");
    }
    Ok(name)
}

/// Without `peer`, waits on `link` for one pairing attempt and prints the
/// code. With `peer`, joins that node with the code it showed.
pub async fn pair(
    link: &str,
    port: u16,
    peer: Option<SocketAddrV4>,
    code: Option<String>,
    me: Option<String>,
    replace: bool,
) -> Result<()> {
    let link = Link::named(link)?;
    let me = name_or_default(me)?;
    let (peer_name, key) = match (peer, code) {
        (Some(peer), Some(code)) => {
            let mut stream = link.connect(peer).await?;
            channel::join(&mut stream, &code, &me, &|peer| {
                !replace && Key::exists(peer)
            })
            .await?
        }
        (None, None) => {
            let listener = link.listen(port)?;
            let code = channel::new_code()?;
            println!(
                "Pairing code: {code}\nOn the other machine, run:\n  fonsviva node pair --link <its interface> --peer {}:{port} --code {code}\nWaiting on {} ({}) for one attempt; a wrong code ends it. This machine's firewall must let port {port} in from the other machine on this link.",
                link.addr, link.interface, link.addr
            );
            let (from, peer, key) = tokio::time::timeout(
                PAIR_WAIT,
                channel::wait_for_join(&listener, &code, &me, &|peer| {
                    !replace && Key::exists(peer)
                }),
            )
            .await
            .context("no one tried to pair within 10 minutes")??;
            drop(listener);
            println!("Paired with {from}");
            (peer, key)
        }
        _ => bail!("give both --peer and --code to join, or neither to wait for one"),
    };
    let path = key.save(&peer_name, replace)?;
    println!(
        "Paired {me} with {peer_name} over {}. Key saved to {} (readable only by you).",
        link.interface,
        path.display()
    );
    Ok(())
}

/// The node side: accepts tunnels from `peer` on `link` and carries each to `to`.
pub async fn serve(link: &str, port: u16, peer: &str, to: SocketAddrV4) -> Result<()> {
    let link = Link::named(link)?;
    let key = Key::load(peer)?;
    let listener = link.listen(port)?;
    println!(
        "Carrying tunnels from {peer} on {}:{port} ({}) to {to}",
        link.addr, link.interface
    );
    channel::serve(listener, key, to).await
}

/// The controller side: listens on `local` and carries each connection to
/// `peer` at `at` over `link`.
pub async fn forward(link: &str, peer: &str, at: SocketAddrV4, local: SocketAddrV4) -> Result<()> {
    if !local.ip().is_loopback() {
        bail!("the local end listens on 127.0.0.1 only");
    }
    let link = Link::named(link)?;
    let key = Key::load(peer)?;
    let listener = TcpListener::bind(local).await?;
    let SocketAddr::V4(bound) = listener.local_addr()? else {
        bail!("not IPv4");
    };
    println!(
        "Carrying {bound} to {peer} at {at} over {} ({})",
        link.interface, link.addr
    );
    channel::forward(listener, link, at, key).await
}

/// A loopback echo server, the target `fonsviva node rtt` measures against.
pub async fn echo(port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    println!("Echoing on {}", listener.local_addr()?);
    loop {
        let (mut conn, _) = listener.accept().await?;
        conn.set_nodelay(true)?;
        tokio::spawn(async move {
            let (mut r, mut w) = conn.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        });
    }
}

/// Round trips of 64 bytes through `to`, which must echo them.
pub async fn rtt(to: SocketAddrV4, count: u32, json: bool) -> Result<()> {
    let mut conn = TcpStream::connect(to)
        .await
        .with_context(|| format!("nothing listens on {to}"))?;
    conn.set_nodelay(true)?;
    let mut buf = [0u8; 64];
    let mut times = Vec::new();
    for i in 0..count.clamp(1, 1_000_000) + 5 {
        let started = Instant::now();
        conn.write_all(&[i as u8; 64]).await?;
        conn.read_exact(&mut buf).await?;
        if i >= 5 {
            times.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    if times.is_empty() {
        bail!("no round trips measured");
    }
    times.sort_by(f64::total_cmp);
    let avg = times.iter().sum::<f64>() / times.len() as f64;
    let p50 = times[times.len() / 2];
    let p99 = times[(times.len() * 99 / 100).min(times.len() - 1)];
    let max = times[times.len() - 1];
    if json {
        println!(
            "{}",
            serde_json::json!({"to": to.to_string(), "count": times.len(), "avg_ms": avg, "p50_ms": p50, "p99_ms": p99, "max_ms": max})
        );
    } else {
        println!(
            "{} round trips of 64 bytes through {to}: avg {avg:.3} ms, p50 {p50:.3}, p99 {p99:.3}, max {max:.3}",
            times.len()
        );
    }
    Ok(())
}
