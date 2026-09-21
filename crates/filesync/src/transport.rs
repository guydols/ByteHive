use crate::protocol::{self, Message, MAX_FRAME_BYTES, SEND_QUEUE_DEPTH};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use log::{debug, warn};
use parking_lot::Mutex;
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

pub type Frame = Arc<Vec<u8>>;

/// True when a session error came from heartbeat liveness (stale peer or
/// suspend/resume gap): TimedOut kind, or a message mentioning "heartbeat".
/// Used for the reconnect fast-path (skip backoff once).
pub fn is_heartbeat_timeout(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::TimedOut || e.to_string().contains("heartbeat")
}
const READ_POLL_MS: u64 = 5;
const RECV_CHANNEL_DEPTH: usize = 1024;

/// App-level heartbeat: send Ping after this long without any outbound
/// traffic, treat the peer as dead after this long without ANY inbound
/// traffic. 30 s timeout tolerates one missed ping + slow disk; suspend
/// recovery is still fast because the wall-clock gap trips the timeout on
/// the next maintenance check after resume.
pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;
pub const HEARTBEAT_TIMEOUT_SECS: u64 = 30;
/// Blocking recv loops poll with this timeout so heartbeat maintenance runs
/// even while idle (also keeps suspend-resume detection latency ≤ 2 s).
pub const HEARTBEAT_POLL_SECS: u64 = 2;

enum TlsStream {
    Server(StreamOwned<ServerConnection, TcpStream>),
    Client(StreamOwned<ClientConnection, TcpStream>),
}

impl TlsStream {
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Server(s) => s.get_ref(),
            Self::Client(s) => s.get_ref(),
        }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Server(s) => s.read(buf),
            Self::Client(s) => s.read(buf),
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Server(s) => s.write(buf),
            Self::Client(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Server(s) => s.flush(),
            Self::Client(s) => s.flush(),
        }
    }
}

struct FrameReader {
    hdr: [u8; 4],
    hdr_pos: usize,
    body: Vec<u8>,
    body_pos: usize,
    body_len: usize,
}

impl FrameReader {
    fn new() -> Self {
        Self {
            hdr: [0u8; 4],
            hdr_pos: 0,
            body: Vec::new(),
            body_pos: 0,
            body_len: 0,
        }
    }

    fn poll(&mut self, r: &mut impl Read) -> io::Result<Option<Message>> {
        while self.hdr_pos < 4 {
            match r.read(&mut self.hdr[self.hdr_pos..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed",
                    ))
                }
                Ok(n) => self.hdr_pos += n,
                Err(e) if is_timeout(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        }

        if self.body.is_empty() {
            let len = u32::from_be_bytes(self.hdr) as usize;
            if len > MAX_FRAME_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("incoming frame {len} B > MAX_FRAME_BYTES {MAX_FRAME_BYTES} B"),
                ));
            }
            self.body_len = len;
            self.body = vec![0u8; len];
            self.body_pos = 0;
        }

        while self.body_pos < self.body_len {
            match r.read(&mut self.body[self.body_pos..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "connection closed",
                    ))
                }
                Ok(n) => self.body_pos += n,
                Err(e) if is_timeout(&e) => return Ok(None),
                Err(e) => return Err(e),
            }
        }

        let raw = lz4_flex::decompress_size_prepended(&self.body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let msg: Message = bincode::deserialize(&raw)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        self.hdr_pos = 0;
        self.body.clear();
        self.body_pos = 0;
        self.body_len = 0;

        Ok(Some(msg))
    }
}

#[inline]
fn is_timeout(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn io_loop(
    mut tls: TlsStream,
    recv_tx: Sender<io::Result<Message>>,
    send_rx: Receiver<Option<Frame>>,
    last_recv_wall: Arc<Mutex<SystemTime>>,
) {
    debug!(
        "tls-io: thread started (send_depth={SEND_QUEUE_DEPTH} recv_depth={RECV_CHANNEL_DEPTH})"
    );
    let mut reader = FrameReader::new();

    loop {
        loop {
            match send_rx.try_recv() {
                Ok(Some(frame)) => {
                    if let Err(e) = tls.write_all(&frame).and_then(|_| tls.flush()) {
                        debug!("tls-io: write error (kind={:?}): {e}", e.kind());
                        let _ = recv_tx.send(Err(e));
                        return;
                    }
                }
                Ok(None) => {
                    debug!("tls-io: shutdown signal received, flushing and exiting");
                    let _ = tls.flush();
                    return;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    debug!("tls-io: send_rx disconnected, exiting");
                    return;
                }
            }
        }

        match reader.poll(&mut tls) {
            Ok(Some(msg)) => {
                // Wall-clock liveness: ANY inbound frame (data, Ping, Pong)
                // proves the peer path is alive. Instant can't be used here —
                // it pauses during suspend so it can't detect resume gaps.
                *last_recv_wall.lock() = SystemTime::now();
                match msg {
                    // Auto-reply in the io thread: zero app churn, no recv
                    // loop match arms need changing. Swallow both so the app
                    // only ever sees data messages.
                    Message::Ping => {
                        match protocol::serialise_message(&Message::Pong) {
                            Ok(frame) => {
                                if let Err(e) =
                                    tls.write_all(&frame).and_then(|_| tls.flush())
                                {
                                    debug!("tls-io: pong write error (kind={:?}): {e}", e.kind());
                                    let _ = recv_tx.send(Err(e));
                                    return;
                                }
                            }
                            Err(e) => {
                                debug!("tls-io: pong serialise error: {e}");
                            }
                        }
                    }
                    Message::Pong => {}
                    msg => {
                        if recv_tx.send(Ok(msg)).is_err() {
                            debug!("tls-io: recv_tx consumer gone, exiting");
                            return;
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                debug!("tls-io: read error (kind={:?}): {e}", e.kind());
                let _ = recv_tx.send(Err(e));
                return;
            }
        }
    }
}

pub struct Connection {
    recv_rx: Mutex<Receiver<io::Result<Message>>>,
    send_tx: Sender<Option<Frame>>,
    last_send_wall: Mutex<SystemTime>,
    last_recv_wall: Arc<Mutex<SystemTime>>,
    pub peer_cert: Option<Vec<u8>>,
}

impl Connection {
    pub fn new_server(
        stream: TcpStream,
        tls_config: Arc<rustls::ServerConfig>,
    ) -> io::Result<Self> {
        let tls_conn = ServerConnection::new(tls_config)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Self::from_tls(TlsStream::Server(StreamOwned::new(tls_conn, stream)))
    }

    pub fn new_client(
        stream: TcpStream,
        tls_config: Arc<rustls::ClientConfig>,
        server_name: rustls::pki_types::ServerName<'static>,
    ) -> io::Result<Self> {
        let tls_conn = ClientConnection::new(tls_config, server_name)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        Self::from_tls(TlsStream::Client(StreamOwned::new(tls_conn, stream)))
    }

    fn from_tls(mut tls: TlsStream) -> io::Result<Self> {
        debug!("tls: completing TLS handshake (blocking)");
        tls.flush()?;
        debug!(
            "tls: TLS handshake complete, setting read poll timeout to {} ms",
            READ_POLL_MS
        );

        tls.tcp()
            .set_read_timeout(Some(Duration::from_millis(READ_POLL_MS)))?;

        let peer_cert: Option<Vec<u8>> = match &tls {
            TlsStream::Server(s) => s
                .conn
                .peer_certificates()
                .and_then(|certs| certs.first())
                .map(|c| c.as_ref().to_vec()),
            TlsStream::Client(s) => s
                .conn
                .peer_certificates()
                .and_then(|certs| certs.first())
                .map(|c| c.as_ref().to_vec()),
        };

        let (recv_tx, recv_rx) = bounded::<io::Result<Message>>(RECV_CHANNEL_DEPTH);
        let (send_tx, send_rx) = bounded::<Option<Frame>>(SEND_QUEUE_DEPTH);

        let now = SystemTime::now();
        let last_recv_wall = Arc::new(Mutex::new(now));
        let last_recv_wall_io = last_recv_wall.clone();
        thread::Builder::new()
            .name("tls-io".into())
            .spawn(move || io_loop(tls, recv_tx, send_rx, last_recv_wall_io))
            .expect("spawn tls-io thread");
        debug!(
            "tls: io-loop thread spawned (recv_channel={RECV_CHANNEL_DEPTH} send_channel={SEND_QUEUE_DEPTH})"
        );

        Ok(Self {
            recv_rx: Mutex::new(recv_rx),
            send_tx,
            last_send_wall: Mutex::new(now),
            last_recv_wall,
            peer_cert,
        })
    }

    fn record_send(&self) {
        *self.last_send_wall.lock() = SystemTime::now();
    }

    pub fn send(&self, msg: &Message) -> io::Result<()> {
        let frame = Arc::new(protocol::serialise_message(msg)?);
        self.send_frame(frame)
    }

    pub fn send_frame(&self, frame: Frame) -> io::Result<()> {
        let queue_used = self.send_tx.len();
        if queue_used >= SEND_QUEUE_DEPTH - 1 {
            warn!(
                "tls-io: send queue nearly full ({}/{}) — TCP backpressure likely; frame={} B",
                queue_used,
                SEND_QUEUE_DEPTH,
                frame.len()
            );
        } else if queue_used > SEND_QUEUE_DEPTH / 2 {
            debug!(
                "tls-io: send queue at {}/{} — frame={} B",
                queue_used,
                SEND_QUEUE_DEPTH,
                frame.len()
            );
        }
        let r = self
            .send_tx
            .send(Some(frame))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"));
        if r.is_ok() {
            self.record_send();
        }
        r
    }

    pub fn try_send_frame(&self, frame: Frame) -> bool {
        match self.send_tx.try_send(Some(frame)) {
            Ok(_) => {
                self.record_send();
                true
            }
            Err(TrySendError::Full(_)) => {
                warn!(
                    "tls-io: try_send_frame dropped — send queue full ({}/{})",
                    self.send_tx.len(),
                    SEND_QUEUE_DEPTH
                );
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                debug!("tls-io: try_send_frame dropped — connection closed");
                false
            }
        }
    }

    pub fn recv(&self) -> io::Result<Message> {
        match self.recv_rx.lock().recv() {
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "connection closed",
            )),
        }
    }

    /// Blocking recv with a timeout. Maps channel disconnect to
    /// ConnectionAborted (same as `recv`) and channel timeout to TimedOut
    /// (== WouldBlock-family via `is_timeout`-style handling at call sites).
    pub fn recv_timeout(&self, t: Duration) -> io::Result<Message> {
        match self.recv_rx.lock().recv_timeout(t) {
            Ok(Ok(msg)) => Ok(msg),
            Ok(Err(e)) => Err(e),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "recv timeout",
            )),
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "connection closed",
            )),
        }
    }

    /// Heartbeat maintenance: send Ping when this side has been idle for
    /// >= HEARTBEAT_INTERVAL_SECS, and fail with TimedOut when nothing has been
    /// received for >= HEARTBEAT_TIMEOUT_SECS (covers dead peers AND
    /// suspend/resume gaps — SystemTime keeps advancing across suspend
    /// while Instant pauses, so the gap trips the timeout on next check).
    /// Backwards clock jumps re-baseline instead of false-tripping.
    /// Only short parking_lot critical sections — never blocks on io_loop
    /// (io_loop only takes last_recv_wall briefly too).
    pub fn heartbeat_maintenance(&self) -> io::Result<()> {
        let now = SystemTime::now();
        let (since_send, since_recv) = {
            let last_send = *self.last_send_wall.lock();
            let last_recv = *self.last_recv_wall.lock();
            match (now.duration_since(last_send), now.duration_since(last_recv)) {
                (Ok(s), Ok(r)) => (Some(s), Some(r)),
                // Clock jumped backwards: re-baseline both, don't trip.
                _ => {
                    *self.last_send_wall.lock() = now;
                    *self.last_recv_wall.lock() = now;
                    return Ok(());
                }
            }
        };
        let since_send = since_send.unwrap();
        let since_recv = since_recv.unwrap();
        if since_send >= Duration::from_secs(HEARTBEAT_INTERVAL_SECS) {
            // send() records last_send on success.
            self.send(&Message::Ping)?;
        }
        if since_recv >= Duration::from_secs(HEARTBEAT_TIMEOUT_SECS) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "heartbeat timeout: no data for {} s",
                    since_recv.as_secs()
                ),
            ));
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        let _ = self.send_tx.send(None);
    }
}
