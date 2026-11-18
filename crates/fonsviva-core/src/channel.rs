//! Node channel: the only way bytes move between machines in a split.
//!
//! Engines listen on 127.0.0.1. Fonsviva carries their bytes over a TCP
//! socket pinned to one link (`IP_BOUND_IF` on macOS, `SO_BINDTODEVICE` on
//! Linux) and encrypted with Noise `NNpsk0`, keyed by the pairing key. A peer
//! without the key fails the first handshake message. Each side sends a
//! heartbeat every second and drops the tunnel after 5 s of silence, so a
//! pulled cable ends the connection instead of hanging it.
//!
//! Pairing derives that key from a one-time code with SPAKE2: the node shows
//! the code, the person types it on the other machine, and the key never
//! crosses the wire or leaves either machine.

use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use snow::{Builder, TransportState};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{Notify, Semaphore};
use tokio::time::timeout;

use crate::topology::{self, Interface};

pub const PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
pub const HEARTBEAT: Duration = Duration::from_secs(1);
pub const DEAD_AFTER: Duration = Duration::from_secs(5);
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const PROLOGUE: &[u8] = b"fonsviva node channel v1";
const MAX_MESSAGE: usize = 65535;
const TAG: usize = 16;
const MAX_CHUNK: usize = MAX_MESSAGE - TAG - 1;
const DATA: u8 = 0;
const BEAT: u8 = 1;
const CLOSE: u8 = 2;
const PAIR_JOINER: &[u8] = b"fonsviva joiner";
const PAIR_NODE: &[u8] = b"fonsviva node";

/// A pairing key. Its `Debug` never shows the bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Key([u8; 32]);

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Key(bytes)
    }

    fn from_pake(secret: &[u8]) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"fonsviva pairing key v1");
        hash.update(secret);
        Key(hash.finalize().into())
    }

    fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn from_hex(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(Key(out))
    }

    /// Loads the key saved for `peer` by [`save`](Key::save).
    pub fn load(peer: &str) -> Result<Self> {
        let path = key_path(peer)?;
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("no pairing with {peer}: run `fonsviva node pair` first"))?;
        Key::from_hex(&text).with_context(|| format!("{} is not a pairing key", path.display()))
    }

    /// Saves the key for `peer`, readable only by this user. An existing
    /// pairing with that name is kept unless `replace` is set.
    pub fn save(&self, peer: &str, replace: bool) -> Result<PathBuf> {
        let path = key_path(peer)?;
        self.save_at(&path, peer, replace)?;
        Ok(path)
    }

    fn save_at(&self, path: &Path, peer: &str, replace: bool) -> Result<()> {
        ensure!(
            replace || !path.exists(),
            "already paired with a machine named {peer}; pair again with --replace to drop that pairing"
        );
        write_private(path, &format!("{}\n", self.to_hex()))
    }
}

fn write_private(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        options.mode(0o600);
        let file = options.open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        (&file).write_all(text.as_bytes())?;
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        options.open(path)?.write_all(text.as_bytes())?;
        Ok(())
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn key_path(peer: &str) -> Result<PathBuf> {
    ensure!(
        valid_name(peer),
        "node name {peer:?} must be lowercase letters, digits and dashes"
    );
    Ok(crate::paths::state_dir()
        .join("nodes")
        .join(format!("{peer}.key")))
}

/// A local interface and its IPv4 address: the link a socket is pinned to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub interface: String,
    pub addr: Ipv4Addr,
}

impl Link {
    /// The link on `interface`, from this machine's addresses.
    pub fn named(interface: &str) -> Result<Self> {
        Link::pick(interface, &topology::local_interfaces())
    }

    pub fn pick(interface: &str, interfaces: &[Interface]) -> Result<Self> {
        let mut found = interfaces.iter().filter(|i| i.name == interface);
        let Some(first) = found.next() else {
            bail!("{interface} has no IPv4 address on this machine");
        };
        ensure!(
            found.next().is_none(),
            "{interface} has several IPv4 addresses; give the one to use"
        );
        Ok(Link {
            interface: first.name.clone(),
            addr: first.addr,
        })
    }

    fn socket(&self) -> Result<TcpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
        self.pin(&socket)?;
        socket.set_nonblocking(true)?;
        socket.set_tcp_nodelay(true)?;
        Ok(TcpSocket::from_std_stream(socket.into()))
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn pin(&self, socket: &Socket) -> Result<()> {
        let name = std::ffi::CString::new(self.interface.as_str())?;
        let index = std::num::NonZeroU32::new(unsafe { libc::if_nametoindex(name.as_ptr()) })
            .with_context(|| format!("no interface named {}", self.interface))?;
        socket
            .bind_device_by_index_v4(Some(index))
            .with_context(|| format!("pinning a socket to {}", self.interface))
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn pin(&self, socket: &Socket) -> Result<()> {
        socket
            .bind_device(Some(self.interface.as_bytes()))
            .with_context(|| format!("pinning a socket to {}", self.interface))
    }

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "linux",
        target_os = "android"
    )))]
    fn pin(&self, _socket: &Socket) -> Result<()> {
        bail!("pinning a socket to a link is not supported on this system")
    }

    /// Listens on this link's address only, pinned to its interface.
    pub fn listen(&self, port: u16) -> Result<TcpListener> {
        let socket = self.socket()?;
        socket.set_reuseaddr(true)?;
        socket
            .bind(SocketAddr::V4(SocketAddrV4::new(self.addr, port)))
            .with_context(|| format!("listening on {}:{port}", self.addr))?;
        Ok(socket.listen(16)?)
    }

    /// Connects to `peer` from this link's address, pinned to its interface.
    pub async fn connect(&self, peer: SocketAddrV4) -> Result<TcpStream> {
        let socket = self.socket()?;
        socket.bind(SocketAddr::V4(SocketAddrV4::new(self.addr, 0)))?;
        let stream = timeout(HANDSHAKE_TIMEOUT, socket.connect(SocketAddr::V4(peer)))
            .await
            .with_context(|| format!("{peer} did not answer on {}", self.interface))?
            .with_context(|| format!("connecting to {peer} over {}", self.interface))?;
        Ok(stream)
    }
}

async fn write_frame(w: &mut (impl AsyncWrite + Unpin), body: &[u8]) -> Result<()> {
    let len = u16::try_from(body.len()).context("frame too large")?;
    let mut out = Vec::with_capacity(2 + body.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(body);
    w.write_all(&out).await?;
    Ok(())
}

async fn read_frame(r: &mut (impl AsyncRead + Unpin), body: &mut Vec<u8>) -> Result<()> {
    let mut len = [0u8; 2];
    r.read_exact(&mut len).await?;
    body.resize(usize::from(u16::from_be_bytes(len)), 0);
    r.read_exact(body).await?;
    Ok(())
}

/// Runs the Noise handshake. A peer holding another key fails here: the
/// responder cannot open the first message.
pub async fn handshake(
    stream: &mut TcpStream,
    key: &Key,
    initiator: bool,
) -> Result<TransportState> {
    timeout(HANDSHAKE_TIMEOUT, async {
        let builder = Builder::new(PATTERN.parse()?)
            .prologue(PROLOGUE)?
            .psk(0, &key.0)?;
        let mut state = if initiator {
            builder.build_initiator()?
        } else {
            builder.build_responder()?
        };
        let mut buf = vec![0u8; MAX_MESSAGE];
        let mut frame = Vec::new();
        if initiator {
            let n = state.write_message(&[], &mut buf)?;
            write_frame(stream, &buf[..n]).await?;
            read_frame(stream, &mut frame).await?;
            state
                .read_message(&frame, &mut buf)
                .context("the peer does not hold this pairing key")?;
        } else {
            read_frame(stream, &mut frame).await?;
            state
                .read_message(&frame, &mut buf)
                .context("the peer does not hold this pairing key")?;
            let n = state.write_message(&[], &mut buf)?;
            write_frame(stream, &buf[..n]).await?;
        }
        Ok(state.into_transport_mode()?)
    })
    .await
    .context("handshake timed out")?
}

fn seal(transport: &Mutex<TransportState>, kind: u8, data: &[u8]) -> Result<Vec<u8>> {
    let mut plain = Vec::with_capacity(1 + data.len());
    plain.push(kind);
    plain.extend_from_slice(data);
    let mut out = vec![0u8; plain.len() + TAG];
    let n = transport
        .lock()
        .map_err(|_| anyhow::anyhow!("channel state poisoned"))?
        .write_message(&plain, &mut out)?;
    out.truncate(n);
    Ok(out)
}

fn open(transport: &Mutex<TransportState>, frame: &[u8]) -> Result<Vec<u8>> {
    let mut out = vec![0u8; frame.len()];
    let n = transport
        .lock()
        .map_err(|_| anyhow::anyhow!("channel state poisoned"))?
        .read_message(frame, &mut out)
        .context("a frame failed to decrypt")?;
    out.truncate(n);
    Ok(out)
}

/// Sends one message inside an established tunnel.
pub async fn send(
    stream: &mut TcpStream,
    transport: &Mutex<TransportState>,
    data: &[u8],
) -> Result<()> {
    ensure!(data.len() <= MAX_CHUNK, "message too large");
    let sealed = seal(transport, DATA, data)?;
    write_frame(stream, &sealed).await
}

/// Receives one message inside an established tunnel.
pub async fn recv(stream: &mut TcpStream, transport: &Mutex<TransportState>) -> Result<Vec<u8>> {
    let mut frame = Vec::new();
    timeout(HANDSHAKE_TIMEOUT, read_frame(stream, &mut frame))
        .await
        .context("the peer went silent")??;
    let mut plain = open(transport, &frame)?;
    ensure!(plain.first() == Some(&DATA), "unexpected frame");
    plain.remove(0);
    Ok(plain)
}

/// Why a relay ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Ended {
    /// Both sides finished sending and each saw the other's close.
    Done,
    /// No frame from the peer for [`DEAD_AFTER`].
    PeerSilent,
    /// The local side took no bytes for [`DEAD_AFTER`] while the peer's waited.
    LocalStalled,
}

#[derive(Default)]
struct Closing {
    local_done: AtomicBool,
    peer_done: AtomicBool,
    both: Notify,
}

impl Closing {
    fn mark(&self, flag: &AtomicBool) {
        flag.store(true, Ordering::SeqCst);
        if self.finished() {
            self.both.notify_one();
        }
    }

    fn finished(&self) -> bool {
        self.local_done.load(Ordering::SeqCst) && self.peer_done.load(Ordering::SeqCst)
    }
}

/// Carries bytes between `local` and the encrypted `tunnel`. Each direction
/// ends with an authenticated close frame, so a half-closed local socket
/// still gets its reply, and a tunnel that ends without one is an error, not
/// a clean finish. The relay ends when both directions have closed, when the
/// peer is silent for [`DEAD_AFTER`], or when the local side refuses bytes for
/// as long; a pulled link is therefore noticed within twice that.
pub async fn relay(
    tunnel: TcpStream,
    local: TcpStream,
    transport: TransportState,
) -> Result<Ended> {
    let transport = Arc::new(Mutex::new(transport));
    let closing = Arc::new(Closing::default());
    let (mut tunnel_read, mut tunnel_write) = tunnel.into_split();
    let (mut local_read, mut local_write) = local.into_split();
    let (sealer, out_closing) = (transport.clone(), closing.clone());
    let outbound = async move {
        let mut chunk = vec![0u8; MAX_CHUNK];
        let mut beat = tokio::time::interval(HEARTBEAT);
        let done = out_closing.both.notified();
        tokio::pin!(done);
        let local_done = |c: &Closing| c.local_done.load(Ordering::SeqCst);
        loop {
            tokio::select! {
                n = local_read.read(&mut chunk), if !local_done(&out_closing) => {
                    let n = n?;
                    if n == 0 {
                        write_frame(&mut tunnel_write, &seal(&sealer, CLOSE, &[])?).await?;
                        out_closing.mark(&out_closing.local_done);
                    } else {
                        write_frame(&mut tunnel_write, &seal(&sealer, DATA, &chunk[..n])?).await?;
                    }
                }
                _ = beat.tick() => {
                    write_frame(&mut tunnel_write, &seal(&sealer, BEAT, &[])?).await?;
                }
                _ = &mut done => {
                    tunnel_write.shutdown().await?;
                    return Ok::<(), anyhow::Error>(());
                }
            }
        }
    };
    let in_closing = closing.clone();
    let inbound = async move {
        let mut frame = Vec::new();
        loop {
            match timeout(DEAD_AFTER, read_frame(&mut tunnel_read, &mut frame)).await {
                Err(_) => return Ok::<Ended, anyhow::Error>(Ended::PeerSilent),
                Ok(Err(_)) if in_closing.finished() => return Ok(Ended::Done),
                Ok(Err(e)) => return Err(e.context("the tunnel ended without a close")),
                Ok(Ok(())) => {}
            }
            let plain = open(&transport, &frame)?;
            match plain.split_first() {
                Some((&DATA, data)) => {
                    ensure!(
                        !in_closing.peer_done.load(Ordering::SeqCst),
                        "data after the peer's close"
                    );
                    if timeout(DEAD_AFTER, local_write.write_all(data))
                        .await
                        .is_err()
                    {
                        return Ok(Ended::LocalStalled);
                    }
                }
                Some((&CLOSE, _)) => {
                    let _ = local_write.shutdown().await;
                    in_closing.mark(&in_closing.peer_done);
                }
                Some((&BEAT, _)) => {}
                _ => bail!("unexpected frame"),
            }
        }
    };
    tokio::pin!(outbound, inbound);
    tokio::select! {
        sent = &mut outbound => {
            sent?;
            match timeout(DEAD_AFTER, &mut inbound).await {
                Ok(ended) => ended,
                Err(_) => Ok(Ended::PeerSilent),
            }
        }
        ended = &mut inbound => ended,
    }
}

const MAX_HANDSHAKES: usize = 32;

async fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    loop {
        match listener.accept().await {
            Ok(accepted) => return accepted,
            Err(e) => {
                eprintln!("fonsviva: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// The node side: accepts tunnels on `listener` and connects each to
/// `target`, which must be on loopback. The engine connection opens only
/// after the first frame decrypts, which a replayed handshake cannot produce.
pub async fn serve(listener: TcpListener, key: Key, target: SocketAddrV4) -> Result<()> {
    ensure!(
        target.ip().is_loopback(),
        "the channel only reaches engines on 127.0.0.1"
    );
    let handshakes = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    loop {
        let (mut tunnel, from) = accept(&listener).await;
        let Ok(permit) = handshakes.clone().try_acquire_owned() else {
            continue;
        };
        let key = key.clone();
        tokio::spawn(async move {
            let result = async {
                tunnel.set_nodelay(true)?;
                let transport = {
                    let _permit = permit;
                    let transport = Mutex::new(handshake(&mut tunnel, &key, false).await?);
                    let mut frame = Vec::new();
                    timeout(HANDSHAKE_TIMEOUT, read_frame(&mut tunnel, &mut frame))
                        .await
                        .context("the peer sent nothing after the handshake")??;
                    ensure!(
                        open(&transport, &frame)?.first() == Some(&BEAT),
                        "the peer did not open with a heartbeat"
                    );
                    transport
                        .into_inner()
                        .map_err(|_| anyhow::anyhow!("channel state poisoned"))?
                };
                let local = TcpStream::connect(target)
                    .await
                    .with_context(|| format!("nothing listens on {target}"))?;
                local.set_nodelay(true)?;
                relay(tunnel, local, transport).await
            }
            .await;
            report(&format!("tunnel from {from}"), result);
        });
    }
}

/// Opens a tunnel to a node: connects over `link`, runs the handshake and
/// sends the heartbeat [`serve`] waits for.
pub async fn open_tunnel(
    link: &Link,
    peer: SocketAddrV4,
    key: &Key,
) -> Result<(TcpStream, TransportState)> {
    let mut tunnel = link.connect(peer).await?;
    let transport = Mutex::new(handshake(&mut tunnel, key, true).await?);
    write_frame(&mut tunnel, &seal(&transport, BEAT, &[])?).await?;
    let transport = transport
        .into_inner()
        .map_err(|_| anyhow::anyhow!("channel state poisoned"))?;
    Ok((tunnel, transport))
}

/// The controller side: accepts local connections on `local` and carries
/// each to `peer` over `link`.
pub async fn forward(local: TcpListener, link: Link, peer: SocketAddrV4, key: Key) -> Result<()> {
    loop {
        let (conn, _) = accept(&local).await;
        let (link, key) = (link.clone(), key.clone());
        tokio::spawn(async move {
            let result = async {
                conn.set_nodelay(true)?;
                let (tunnel, transport) = open_tunnel(&link, peer, &key).await?;
                relay(tunnel, conn, transport).await
            }
            .await;
            report(&format!("tunnel to {peer}"), result);
        });
    }
}

fn report(what: &str, result: Result<Ended>) {
    match result {
        Ok(Ended::Done) => {}
        Ok(Ended::PeerSilent) => eprintln!(
            "fonsviva: {what} closed: no heartbeat for {} s",
            DEAD_AFTER.as_secs()
        ),
        Ok(Ended::LocalStalled) => eprintln!(
            "fonsviva: {what} closed: the local side took no bytes for {} s",
            DEAD_AFTER.as_secs()
        ),
        Err(e) => eprintln!("fonsviva: {what} failed: {e:#}"),
    }
}

/// A one-time pairing code: 10 random digits, shown as `123-456-7890`.
pub fn new_code() -> Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    let n = u64::from_le_bytes(bytes) % 10_000_000_000;
    let digits = format!("{n:010}");
    Ok(format!(
        "{}-{}-{}",
        &digits[..3],
        &digits[3..6],
        &digits[6..]
    ))
}

fn code_digits(code: &str) -> Result<String> {
    let digits: String = code.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
    ensure!(
        digits.len() == 10 && digits.bytes().all(|b| b.is_ascii_digit()),
        "a pairing code is 10 digits"
    );
    Ok(digits)
}

const OPENING_LEN: usize = 33;

fn is_opening(frame: &[u8]) -> bool {
    frame.len() == OPENING_LEN && frame[0] == b'A'
}

/// Joins a node that is waiting with `code`. Returns the node's name and the
/// key; saves nothing.
pub async fn join(stream: &mut TcpStream, code: &str, me: &str) -> Result<(String, Key)> {
    pair(stream, code, me, None).await
}

/// Waits on `listener` for one pairing attempt with `code`. Each connection's
/// opening is read on its own task, so a client that connects and says
/// nothing cannot hold up the real one. A connection that does not open with
/// a SPAKE2 message (a port scan, a local service probing new ports) is
/// dropped and the wait goes on; the first real attempt ends it, matched or
/// not, so a wrong code cannot be retried.
pub async fn wait_for_join(
    listener: &TcpListener,
    code: &str,
    me: &str,
) -> Result<(SocketAddr, String, Key)> {
    code_digits(code)?;
    let (found, mut openings) = tokio::sync::mpsc::channel(1);
    let reading = Arc::new(Semaphore::new(MAX_HANDSHAKES));
    loop {
        tokio::select! {
            (mut stream, from) = accept(listener) => {
                let Ok(permit) = reading.clone().try_acquire_owned() else {
                    continue;
                };
                let found = found.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let mut opening = Vec::new();
                    let opened = timeout(HANDSHAKE_TIMEOUT, read_frame(&mut stream, &mut opening)).await;
                    if matches!(opened, Ok(Ok(()))) && is_opening(&opening) {
                        let _ = found.try_send((stream, from, opening));
                    }
                });
            }
            Some((mut stream, from, opening)) = openings.recv() => {
                let (peer, key) = pair(&mut stream, code, me, Some(opening)).await?;
                return Ok((from, peer, key));
            }
        }
    }
}

/// Runs SPAKE2 with `code`, confirms the key with a Noise handshake, and
/// trades names inside it. The joiner passes no `opening`; the node passes
/// the joiner's first message, already read.
async fn pair(
    stream: &mut TcpStream,
    code: &str,
    me: &str,
    opening: Option<Vec<u8>>,
) -> Result<(String, Key)> {
    use spake2::{Ed25519Group, Identity, Password, Spake2};
    ensure!(
        valid_name(me),
        "node name {me:?} must be lowercase letters, digits and dashes"
    );
    let joiner = opening.is_none();
    let password = Password::new(code_digits(code)?.into_bytes());
    let (joiner_id, node_id) = (Identity::new(PAIR_JOINER), Identity::new(PAIR_NODE));
    let (pake, outbound) = if joiner {
        Spake2::<Ed25519Group>::start_a(&password, &joiner_id, &node_id)
    } else {
        Spake2::<Ed25519Group>::start_b(&password, &joiner_id, &node_id)
    };
    let inbound = timeout(HANDSHAKE_TIMEOUT, async {
        write_frame(stream, &outbound).await?;
        match opening {
            Some(opening) => Ok::<Vec<u8>, anyhow::Error>(opening),
            None => {
                let mut inbound = Vec::new();
                read_frame(stream, &mut inbound).await?;
                Ok(inbound)
            }
        }
    })
    .await
    .context("pairing timed out")??;
    let secret = pake
        .finish(&inbound)
        .map_err(|e| anyhow::anyhow!("pairing failed: {e:?}"))?;
    let key = Key::from_pake(&secret);
    let transport = Mutex::new(
        handshake(stream, &key, joiner)
            .await
            .context("the code did not match")?,
    );
    let peer = if joiner {
        send(stream, &transport, me.as_bytes()).await?;
        recv(stream, &transport).await?
    } else {
        let peer = recv(stream, &transport).await?;
        send(stream, &transport, me.as_bytes()).await?;
        peer
    };
    let peer = String::from_utf8(peer).context("peer name")?;
    ensure!(valid_name(&peer), "the peer sent a bad name");
    Ok((peer, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn key(byte: u8) -> Key {
        Key([byte; 32])
    }

    async fn echo() -> SocketAddrV4 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let (mut r, mut w) = conn.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        addr
    }

    async fn node(server_key: Key, target: SocketAddrV4) -> SocketAddrV4 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        tokio::spawn(serve(listener, server_key, target));
        addr
    }

    async fn tunnel(node: SocketAddrV4, key: &Key) -> Result<(TcpStream, TransportState)> {
        let mut stream = TcpStream::connect(node).await?;
        let transport = Mutex::new(handshake(&mut stream, key, true).await?);
        write_frame(&mut stream, &seal(&transport, BEAT, &[])?).await?;
        Ok((stream, transport.into_inner().unwrap()))
    }

    async fn reply_after_eof() -> SocketAddrV4 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(addr) = listener.local_addr().unwrap() else {
            unreachable!()
        };
        tokio::spawn(async move {
            loop {
                let (mut conn, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    conn.read_to_end(&mut request).await.unwrap();
                    request.reverse();
                    conn.write_all(&request).await.unwrap();
                });
            }
        });
        addr
    }

    async fn raw_peer(
        key: Key,
        then: impl FnOnce(TcpStream, Mutex<TransportState>) -> tokio::task::JoinHandle<()>
        + Send
        + 'static,
    ) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let transport = handshake(&mut stream, &key, false).await.unwrap();
            then(stream, Mutex::new(transport)).await.unwrap();
        });
        addr
    }

    async fn relay_to(peer: SocketAddr, local: TcpStream) -> Result<Ended> {
        let mut stream = TcpStream::connect(peer).await.unwrap();
        let transport = handshake(&mut stream, &key(7), true).await.unwrap();
        relay(stream, local, transport).await
    }

    async fn local_end(node: SocketAddrV4, key: Key) -> TcpStream {
        let (tunnel, transport) = tunnel(node, &key).await.unwrap();
        let (mine, theirs) = loopback_pair().await;
        tokio::spawn(relay(tunnel, theirs, transport));
        mine
    }

    async fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (a, b) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (a.unwrap(), b.unwrap().0)
    }

    #[tokio::test]
    async fn bytes_cross_the_channel_both_ways() {
        let node = node(key(7), echo().await).await;
        let mut conn = local_end(node, key(7)).await;
        let big: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        conn.write_all(&big).await.unwrap();
        let mut back = vec![0u8; big.len()];
        conn.read_exact(&mut back).await.unwrap();
        assert_eq!(back, big);
    }

    #[tokio::test]
    async fn a_peer_with_another_key_is_refused() {
        let (mut a, mut b) = loopback_pair().await;
        let (right, wrong) = (key(7), key(8));
        let (node, peer) = tokio::join!(
            handshake(&mut a, &right, false),
            handshake(&mut b, &wrong, true)
        );
        assert!(format!("{:#}", node.unwrap_err()).contains("pairing key"));
        assert!(peer.is_err());
    }

    #[tokio::test]
    async fn a_half_closed_client_still_gets_its_whole_reply() {
        for target in [echo().await, reply_after_eof().await] {
            let node = node(key(7), target).await;
            let mut conn = local_end(node, key(7)).await;
            let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
            conn.write_all(&big).await.unwrap();
            conn.shutdown().await.unwrap();
            let mut back = Vec::new();
            timeout(Duration::from_secs(10), conn.read_to_end(&mut back))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(back.len(), big.len());
            assert!(back == big || back.iter().rev().eq(big.iter()));
        }
    }

    #[tokio::test]
    async fn a_local_side_that_stops_reading_ends_the_relay() {
        let peer = raw_peer(key(7), |mut stream, transport| {
            tokio::spawn(async move {
                let chunk = vec![1u8; 60_000];
                for _ in 0..2_000 {
                    let Ok(frame) = seal(&transport, DATA, &chunk) else {
                        return;
                    };
                    if write_frame(&mut stream, &frame).await.is_err() {
                        return;
                    }
                }
            })
        })
        .await;
        let (mine, theirs) = loopback_pair().await;
        socket2::SockRef::from(&mine)
            .set_recv_buffer_size(4096)
            .unwrap();
        socket2::SockRef::from(&theirs)
            .set_send_buffer_size(4096)
            .unwrap();
        let started = Instant::now();
        let ended = timeout(Duration::from_secs(20), relay_to(peer, theirs))
            .await
            .unwrap();
        assert_eq!(ended.unwrap(), Ended::LocalStalled);
        assert!(
            started.elapsed() < DEAD_AFTER + Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_tunnel_cut_without_a_close_is_an_error() {
        let peer = raw_peer(key(7), |stream, _| {
            tokio::spawn(async move { drop(stream) })
        })
        .await;
        let (_mine, theirs) = loopback_pair().await;
        let err = relay_to(peer, theirs).await.unwrap_err();
        assert!(format!("{err:#}").contains("without a close"), "{err:#}");
    }

    #[tokio::test]
    async fn a_replayed_first_message_opens_no_engine_connection() {
        let engine = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let SocketAddr::V4(target) = engine.local_addr().unwrap() else {
            unreachable!()
        };
        let node = node(key(7), target).await;
        let mut first = vec![0u8; MAX_MESSAGE];
        let mut initiator = Builder::new(PATTERN.parse().unwrap())
            .prologue(PROLOGUE)
            .unwrap()
            .psk(0, &key(7).0)
            .unwrap()
            .build_initiator()
            .unwrap();
        let n = initiator.write_message(&[], &mut first).unwrap();
        for _ in 0..2 {
            let mut replay = TcpStream::connect(node).await.unwrap();
            write_frame(&mut replay, &first[..n]).await.unwrap();
            let mut answer = Vec::new();
            read_frame(&mut replay, &mut answer).await.unwrap();
            write_frame(&mut replay, &[0u8; 40]).await.unwrap();
        }
        assert!(
            timeout(Duration::from_secs(1), engine.accept())
                .await
                .is_err()
        );
    }

    #[test]
    fn a_second_pairing_with_the_same_name_needs_replace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes/parish1.key");
        key(1).save_at(&path, "parish1", false).unwrap();
        assert!(key(2).save_at(&path, "parish1", false).is_err());
        assert_eq!(
            Key::from_hex(&std::fs::read_to_string(&path).unwrap()),
            Some(key(1))
        );
        key(2).save_at(&path, "parish1", true).unwrap();
        assert_eq!(
            Key::from_hex(&std::fs::read_to_string(&path).unwrap()),
            Some(key(2))
        );
    }

    #[tokio::test]
    async fn a_plain_connection_reaches_nothing() {
        let node = node(key(7), echo().await).await;
        let mut conn = TcpStream::connect(node).await.unwrap();
        conn.write_all(b"\x00\x05hello").await.unwrap();
        let mut buf = [0u8; 64];
        let n = timeout(Duration::from_secs(2), conn.read(&mut buf))
            .await
            .unwrap();
        assert!(
            matches!(n, Ok(0) | Err(_)),
            "the node answered a plain client: {n:?}"
        );
    }

    #[tokio::test]
    async fn the_channel_refuses_a_target_off_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let err = serve(listener, key(7), "10.0.0.5:50052".parse().unwrap())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn a_silent_peer_ends_the_relay_within_the_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let silent = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _transport = handshake(&mut stream, &key(7), false).await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(stream);
        });
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let transport = handshake(&mut stream, &key(7), true).await.unwrap();
        let (_mine, theirs) = loopback_pair().await;
        let started = Instant::now();
        let ended = relay(stream, theirs, transport).await.unwrap();
        assert_eq!(ended, Ended::PeerSilent);
        let took = started.elapsed();
        assert!(
            took >= DEAD_AFTER && took < DEAD_AFTER + Duration::from_secs(2),
            "{took:?}"
        );
        silent.abort();
    }

    async fn waiting_node(
        code: &str,
    ) -> (
        SocketAddr,
        tokio::task::JoinHandle<Result<(SocketAddr, String, Key)>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let code = code.to_string();
        let task = tokio::spawn(async move { wait_for_join(&listener, &code, "parish1").await });
        (addr, task)
    }

    #[tokio::test]
    async fn pairing_gives_both_sides_the_same_key_and_names() {
        let code = new_code().unwrap();
        let (addr, node) = waiting_node(&code).await;
        let mut probe = TcpStream::connect(addr).await.unwrap();
        probe
            .write_all(b"GET /v1/models HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        drop(probe);
        drop(TcpStream::connect(addr).await.unwrap());
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let (node_name, joiner_key) = join(&mut stream, &code, "mac").await.unwrap();
        let (_, joiner_name, node_key) = node.await.unwrap().unwrap();
        assert_eq!(
            (node_name.as_str(), joiner_name.as_str()),
            ("parish1", "mac")
        );
        assert_eq!(node_key, joiner_key);
    }

    #[tokio::test]
    async fn a_wrong_code_ends_the_wait_and_pairs_nothing() {
        let (addr, node) = waiting_node("111-111-1111").await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        assert!(join(&mut stream, "111-111-1112", "mac").await.is_err());
        assert!(node.await.unwrap().is_err());
    }

    #[test]
    fn codes_are_ten_digits_and_keys_round_trip_as_hex() {
        let code = new_code().unwrap();
        assert_eq!(code_digits(&code).unwrap().len(), 10);
        assert!(code_digits("12-34").is_err());
        let key = Key::from_pake(b"secret");
        assert_eq!(Key::from_hex(&key.to_hex()), Some(key.clone()));
        assert_eq!(format!("{key:?}"), "Key(..)");
        assert!(!valid_name("../etc") && valid_name("parish1"));
    }

    #[test]
    fn a_link_needs_exactly_one_address() {
        let interfaces = topology::parse_ifconfig(
            "bridge0: flags=8863<UP> mtu 1500
\tinet 169.254.87.94 netmask 0xffff0000 broadcast 169.254.255.255
",
        );
        assert_eq!(
            Link::pick("bridge0", &interfaces).unwrap().addr,
            "169.254.87.94".parse::<Ipv4Addr>().unwrap()
        );
        assert!(Link::pick("en9", &interfaces).is_err());
    }
}
