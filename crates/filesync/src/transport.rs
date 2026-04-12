use crate::protocol::{self, Message, MAX_FRAME_BYTES, SEND_QUEUE_DEPTH};
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError, TrySendError};
use log::{debug, warn};
use parking_lot::Mutex;
use rustls::{ClientConnection, ServerConnection, StreamOwned};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

pub type Frame = Arc<Vec<u8>>;
const READ_POLL_MS: u64 = 5;
const RECV_CHANNEL_DEPTH: usize = 1024;

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
) {
    // By the time this runs the TLS handshake has already been completed
    // in from_tls(), so complete_prior_io() inside write/flush will never
    // see is_handshaking() == true and will never try to read from the
    // socket as part of a write.  WouldBlock can therefore only come from
    // the deliberate short read-poll timeout, which FrameReader::poll
    // already handles correctly.
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
                if recv_tx.send(Ok(msg)).is_err() {
                    debug!("tls-io: recv_tx consumer gone, exiting");
                    return;
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
        // The TLS handshake MUST be completed here, in blocking mode, before
        // the short read-poll timeout is applied.
        //
        // Root cause of the connection bug
        // ---------------------------------
        // rustls's Stream::write() calls complete_prior_io() first.  If the
        // handshake is still in progress (is_handshaking() == true),
        // complete_prior_io() calls ConnectionCommon::complete_io(), which
        // tries to read the peer's next handshake record.  With the 5 ms poll
        // timeout already set, that read returns EAGAIN (WouldBlock / os error
        // 11) after 5 ms if the peer hasn't replied yet.  complete_io()
        // propagates that error immediately (no retry), so write_all() —and
        // therefore every send — fails with "Resource temporarily unavailable"
        // before any application data is exchanged.  The peer then sees an
        // unexpected EOF as the connection drops.
        //
        // The fix
        // -------
        // flush() on a freshly created StreamOwned drives complete_io() with
        // until_handshaked = true.  With a blocking socket (the OS default for
        // a newly connected/accepted TcpStream) that loop runs until
        // is_handshaking() == false, i.e. until the full handshake is done.
        // Only then do we install the short poll timeout that io_loop needs to
        // interleave reads and writes without dedicated threads.  After this
        // point complete_prior_io() in write/flush will never enter the
        // is_handshaking() branch, so WouldBlock can only arise from the
        // deliberate poll timeout on reads — and FrameReader::poll handles
        // that correctly already.
        tls.flush()?;
        debug!(
            "tls: TLS handshake complete, setting read poll timeout to {} ms",
            READ_POLL_MS
        );

        tls.tcp()
            .set_read_timeout(Some(Duration::from_millis(READ_POLL_MS)))?;

        let (recv_tx, recv_rx) = bounded::<io::Result<Message>>(RECV_CHANNEL_DEPTH);
        let (send_tx, send_rx) = bounded::<Option<Frame>>(SEND_QUEUE_DEPTH);

        thread::Builder::new()
            .name("tls-io".into())
            .spawn(move || io_loop(tls, recv_tx, send_rx))
            .expect("spawn tls-io thread");
        debug!(
            "tls: io-loop thread spawned (recv_channel={RECV_CHANNEL_DEPTH} send_channel={SEND_QUEUE_DEPTH})"
        );

        Ok(Self {
            recv_rx: Mutex::new(recv_rx),
            send_tx,
        })
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
        self.send_tx
            .send(Some(frame))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"))
    }

    pub fn try_send_frame(&self, frame: Frame) -> bool {
        match self.send_tx.try_send(Some(frame)) {
            Ok(_) => true,
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

    pub fn shutdown(&self) {
        let _ = self.send_tx.send(None);
    }
}
