//! Tunnel channel: frame demultiplexing over one TLS connection plus the
//! per-stream relay pump.
//!
//! Threading model: the daemon runs everything on a single-threaded
//! `LocalSet`, so the shared channel state uses `Rc`/`RefCell` — no
//! cross-thread plumbing needed.
//!
//! Flow control follows the approved design: each direction of a stream has
//! a 256KB credit window. The relay reads from its local socket only while
//! `outstanding + chunk ≤ window`; the peer acknowledges consumed bytes
//! cumulatively. Because the relay only reads when credit allows, a stalled
//! peer propagates TCP backpressure all the way back to the local
//! application — no unbounded user-space buffering (the 2c43cd97 lesson).
//!
//! Stream ids are split by role: the initiating side (forward) allocates
//! odd ids, the responding side (exit) even ids — the collision-free split
//! that dde-cooperation had to retrofit (28091a7b).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::TlsStream;

use super::frames::{self, CREDIT_WINDOW, Frame, MAX_DATA_CHUNK, Open};

/// Hard cap on simultaneously open streams per channel. OPENs beyond this
/// are rejected with `OPEN_ACK{ok:false}`.
pub const MAX_STREAMS_PER_CHANNEL: usize = 64;

/// Per-stream inbound frame buffer. Legitimate peers cannot exceed the
/// credit window in flight (16 frames of 16KB), so this never fills under
/// correct behavior; it only exists to bound a misbehaving peer.
const INBOX_CAPACITY: usize = 64;

const WRITER_CAPACITY: usize = 256;

/// Which side of the tunnel this channel is on; determines the stream-id
/// parity we allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// Dials the exit and opens streams (restricted side, odd ids).
    Initiator,
    /// Accepts OPENs and dials real targets (exit side, even ids).
    Responder,
}

/// Events surfaced by the channel reader to the side that receives them.
pub enum ChannelEvent {
    /// Responder side only: the peer asked for a new stream.
    Open { stream: u32, open: Open },
    /// The peer closed the channel (EOF / protocol error). All stream
    /// inboxes have been dropped; every relay unwinds on its own.
    Closed,
}

struct ChannelInner {
    tx: mpsc::Sender<Frame>,
    inboxes: RefCell<HashMap<u32, mpsc::Sender<Frame>>>,
    next_id: Cell<u32>,
    tasks: RefCell<Vec<tokio::task::JoinHandle<()>>>,
}

/// Cloneable handle to one tunnel channel.
#[derive(Clone)]
pub struct Channel {
    inner: Rc<ChannelInner>,
}

pub struct ChannelParts {
    pub channel: Channel,
    pub events: mpsc::Receiver<ChannelEvent>,
}

impl Channel {
    /// Split the TLS stream into writer/reader tasks and return the
    /// handle plus the event receiver. `side` decides id parity. Accepts
    /// either concrete TLS stream half (server/client) or the unified
    /// enum, all via `Into`.
    pub fn spawn(stream: impl Into<TlsStream<TcpStream>>, side: Side) -> ChannelParts {
        let stream: TlsStream<TcpStream> = stream.into();
        let (tx, writer_rx) = mpsc::channel(WRITER_CAPACITY);
        let (events_tx, events_rx) = mpsc::channel(16);
        let id_base = match side {
            Side::Initiator => 1,
            Side::Responder => 2,
        };
        let inner = Rc::new(ChannelInner {
            tx,
            inboxes: RefCell::new(HashMap::new()),
            next_id: Cell::new(id_base),
            tasks: RefCell::new(Vec::new()),
        });

        let (read_half, write_half) = tokio::io::split(stream);
        let writer = tokio::task::spawn_local(writer_task(write_half, writer_rx));
        let reader = tokio::task::spawn_local(reader_task(
            read_half,
            inner.clone(),
            events_tx,
            side == Side::Responder,
        ));
        *inner.tasks.borrow_mut() = vec![writer, reader];

        ChannelParts {
            channel: Channel { inner },
            events: events_rx,
        }
    }

    /// Next stream id for this side (parity-fixed, never reused while the
    /// channel lives).
    pub fn alloc_id(&self) -> u32 {
        let id = self.inner.next_id.get();
        self.inner.next_id.set(id.wrapping_add(2));
        id
    }

    /// Number of streams currently attached.
    pub fn stream_count(&self) -> usize {
        self.inner.inboxes.borrow().len()
    }

    /// Whether `id` is already attached (duplicate-OPEN guard).
    pub fn is_attached(&self, id: u32) -> bool {
        self.inner.inboxes.borrow().contains_key(&id)
    }

    /// Register an inbox for `id`. Frames the reader routes to it are
    /// buffered in order until `recv`ed.
    pub fn attach(&self, id: u32) -> mpsc::Receiver<Frame> {
        let (tx, rx) = mpsc::channel(INBOX_CAPACITY);
        self.inner.inboxes.borrow_mut().insert(id, tx);
        rx
    }

    /// Drop `id`'s inbox: the relay holding the receiver observes `None`
    /// on its next recv and unwinds.
    pub fn detach(&self, id: u32) {
        self.inner.inboxes.borrow_mut().remove(&id);
    }

    /// Abort the channel: kill both IO tasks and release every inbox.
    /// Relays observe `None` and unwind; the underlying socket drops as
    /// the task halves are dropped.
    pub fn close(&self) {
        for task in self.inner.tasks.borrow().iter() {
            task.abort();
        }
        self.inner.tasks.borrow_mut().clear();
        self.inner.inboxes.borrow_mut().clear();
    }

    /// Queue a frame for the peer. Fails only when the writer task is gone
    /// (channel dead).
    pub async fn send(&self, frame: Frame) -> Result<(), mpsc::error::SendError<Frame>> {
        self.inner.tx.send(frame).await
    }
}

async fn writer_task(mut write: WriteHalf<TlsStream<TcpStream>>, mut rx: mpsc::Receiver<Frame>) {
    let mut out = Vec::with_capacity(frames::MAX_FRAME_PAYLOAD + 9);
    while let Some(frame) = rx.recv().await {
        out.clear();
        if frames::encode(&frame, &mut out).is_err() {
            log::warn!("tunnel: dropping unencodable frame {frame}");
            continue;
        }
        if write.write_all(&out).await.is_err() || write.flush().await.is_err() {
            log::debug!("tunnel: write half closed ({frame})");
            return;
        }
    }
}

async fn reader_task(
    mut read: ReadHalf<TlsStream<TcpStream>>,
    channel: Rc<ChannelInner>,
    events: mpsc::Sender<ChannelEvent>,
    accepts_open: bool,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let result = 'read: loop {
        match read.read(&mut chunk).await {
            Ok(0) => break 'read Ok(()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => {
                log::debug!("tunnel: read error: {e}");
                break 'read Err(e);
            }
        }
        loop {
            match frames::decode(&mut buf) {
                Ok(Some(frame)) => {
                    if !route(&channel, frame, &events, accepts_open).await {
                        break 'read Ok(());
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::warn!("tunnel: protocol violation, closing channel: {e}");
                    break 'read Err(std::io::Error::other(e));
                }
            }
        }
    };
    if let Err(e) = result {
        log::debug!("tunnel channel reader ended: {e}");
    }
    // Dropping the inboxes releases every relay: their `recv()` yields
    // `None` and they close their local sockets.
    channel.inboxes.borrow_mut().clear();
    let _ = events.send(ChannelEvent::Closed).await;
}

/// Route one decoded frame. Returns false when the reader must stop
/// (event receiver gone — the channel owner hung up).
async fn route(
    channel: &Rc<ChannelInner>,
    frame: Frame,
    events: &mpsc::Sender<ChannelEvent>,
    accepts_open: bool,
) -> bool {
    match frame {
        Frame::Open { stream, open } if accepts_open => events
            .send(ChannelEvent::Open { stream, open })
            .await
            .is_ok(),
        Frame::Open { stream, .. } => {
            log::debug!("tunnel: ignoring unexpected OPEN {stream} on this side");
            true
        }
        Frame::OpenAck { stream, .. } if accepts_open => {
            log::debug!("tunnel: ignoring unexpected OPEN_ACK {stream} on exit side");
            true
        }
        frame => {
            let id = frame.stream();
            // Clone the sender out of the RefCell before awaiting: holding
            // the borrow across `inbox.send(...).await` would panic if the
            // reader woke while another borrow was active.
            let inbox = channel.inboxes.borrow().get(&id).cloned();
            match inbox {
                Some(inbox) => inbox.send(frame).await.is_ok(),
                None => match frame {
                    // Data for a dead/unknown stream: tell the peer to stop.
                    Frame::Data { .. } => {
                        let _ = channel.tx.send(Frame::Close { stream: id }).await;
                        true
                    }
                    // Stale Ack/Close/OpenAck for a torn-down stream: harmless.
                    _ => true,
                },
            }
        }
    }
}

/// Relay one established stream between a local TCP socket and the channel.
///
/// Half-close mapping: local EOF → `CLOSE` frame; received `CLOSE` →
/// shutdown of the local write half. The relay finishes once both
/// directions are closed (or the channel died).
pub async fn relay(
    sock: TcpStream,
    channel: Channel,
    stream: u32,
    mut inbox: mpsc::Receiver<Frame>,
) {
    let (mut rd, mut wr) = sock.into_split();
    // Bytes sent to the peer and not yet acknowledged (credit window).
    let mut outstanding: u64 = 0;
    // Consumed bytes received from the peer, acknowledged cumulatively.
    let mut to_ack: u32 = 0;
    let mut read_open = true;
    let mut write_open = true;
    let mut buf = vec![0u8; MAX_DATA_CHUNK];

    loop {
        if !read_open && !write_open {
            break;
        }
        tokio::select! {
            biased;
            frame = inbox.recv() => {
                let Some(frame) = frame else {
                    // Channel dead or we were detached: stop relaying. The
                    // socket is dropped whole — a half-open tunnel with no
                    // channel is worthless.
                    break;
                };
                match frame {
                    Frame::Data { stream: s, data } if s == stream => {
                        if write_open && wr.write_all(&data).await.is_ok() {
                            to_ack = to_ack.saturating_add(data.len() as u32);
                            let ack = Frame::Ack { stream, consumed: to_ack };
                            if channel.send(ack).await.is_err() {
                                break;
                            }
                        } else {
                            break;
                        }
                    }
                    Frame::Ack { stream: s, consumed } if s == stream => {
                        outstanding = outstanding.saturating_sub(u64::from(consumed));
                    }
                    Frame::Close { stream: s } if s == stream => {
                        write_open = false;
                        let _ = wr.shutdown().await;
                    }
                    // Frame for a torn-down id or an unexpected type: ignore.
                    _ => {}
                }
            }
            n = rd.read(&mut buf), if read_open && outstanding + MAX_DATA_CHUNK as u64 <= CREDIT_WINDOW => {
                match n {
                    Ok(0) => {
                        read_open = false;
                        if channel.send(Frame::Close { stream }).await.is_err() {
                            break;
                        }
                    }
                    Ok(n) => {
                        let data = Frame::Data { stream, data: buf[..n].to_vec() };
                        if channel.send(data).await.is_err() {
                            break;
                        }
                        outstanding += n as u64;
                    }
                    Err(e) if is_abrupt_reset(&e) => {
                        // The application went away without FIN (RST): treat
                        // as EOF for the read direction.
                        read_open = false;
                        let _ = channel.send(Frame::Close { stream }).await;
                    }
                    Err(_) => break,
                }
            }
        }
    }
    channel.detach(stream);
}

fn is_abrupt_reset(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::frames::OpenAck;
    use std::time::Duration;

    async fn tcp_pair() -> (
        tokio_rustls::client::TlsStream<TcpStream>,
        tokio_rustls::server::TlsStream<TcpStream>,
    ) {
        // Real loopback sockets; the "TLS" is simulated by using the raw
        // TcpStreams cast through a thin wrapper — but TlsStream is a real
        // type, so instead both sides run actual rustls with the shared
        // self-signed test cert (cheap: one in-process handshake).
        let cert = webrtc_dtls::crypto::Certificate::generate_self_signed(["ch".to_owned()])
            .expect("cert");
        let server_cfg = std::sync::Arc::new(super::super::tls::server_config(&cert).expect("cfg"));
        let client_cfg = std::sync::Arc::new(super::super::tls::client_config(&cert).expect("cfg"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept");
            tokio_rustls::TlsAcceptor::from(server_cfg)
                .accept(sock)
                .await
                .expect("tls")
        });
        let sock = TcpStream::connect(addr).await.expect("tcp");
        let client = tokio_rustls::TlsConnector::from(client_cfg)
            .connect(
                rustls::pki_types::ServerName::try_from("ignored").unwrap(),
                sock,
            )
            .await
            .expect("tls");
        (client, server.await.expect("server"))
    }

    #[tokio::test]
    async fn open_routes_to_responder_and_ack_back() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client_tls, server_tls) = tcp_pair().await;
                let ChannelParts {
                    channel: initiator,
                    events: _init_events,
                } = Channel::spawn(client_tls, Side::Initiator);
                let ChannelParts {
                    channel: responder,
                    events: mut resp_events,
                } = Channel::spawn(server_tls, Side::Responder);

                let id = initiator.alloc_id(); // 1 (odd)
                let mut inbox = initiator.attach(id);
                initiator
                    .send(Frame::Open {
                        stream: id,
                        open: Open {
                            target: "127.0.0.1".into(),
                            port: 80,
                            nonce: 42,
                        },
                    })
                    .await
                    .expect("send");

                let event =
                    tokio::time::timeout(std::time::Duration::from_secs(2), resp_events.recv())
                        .await
                        .expect("open event in time")
                        .expect("event");
                match event {
                    ChannelEvent::Open { stream, open } => {
                        assert_eq!(stream, id);
                        assert_eq!(open.nonce, 42);
                        // even-id allocation on the responder
                        assert_eq!(responder.alloc_id(), 2);
                        responder
                            .send(Frame::OpenAck {
                                stream,
                                ack: OpenAck {
                                    ok: true,
                                    nonce: 42,
                                    reason: String::new(),
                                },
                            })
                            .await
                            .expect("ack");
                    }
                    ChannelEvent::Closed => panic!("channel died"),
                }
                let ack = tokio::time::timeout(Duration::from_secs(2), inbox.recv())
                    .await
                    .expect("ack in time")
                    .expect("frame");
                assert!(matches!(ack, Frame::OpenAck { stream, .. } if stream == id));
            })
            .await;
    }

    #[tokio::test]
    async fn data_for_unknown_stream_gets_close_back() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client_tls, server_tls) = tcp_pair().await;
                let ChannelParts {
                    channel: initiator, ..
                } = Channel::spawn(client_tls, Side::Initiator);
                let ChannelParts {
                    channel: _responder,
                    ..
                } = Channel::spawn(server_tls, Side::Responder);

                let ghost = 99; // the responder never attached this id
                let mut inbox = initiator.attach(ghost);
                // The initiator sends DATA for a stream the responder doesn't
                // know; the responder's reader must answer CLOSE for it.
                initiator
                    .send(Frame::Data {
                        stream: ghost,
                        data: b"hi".to_vec(),
                    })
                    .await
                    .expect("send");
                let frame = tokio::time::timeout(Duration::from_secs(2), inbox.recv())
                    .await
                    .expect("in time")
                    .expect("frame");
                assert!(
                    matches!(frame, Frame::Close { stream } if stream == ghost),
                    "expected CLOSE {ghost}, got {frame:?}"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn channel_death_releases_inboxes() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (client_tls, server_tls) = tcp_pair().await;
                let ChannelParts {
                    channel: initiator,
                    mut events,
                } = Channel::spawn(client_tls, Side::Initiator);
                let ChannelParts {
                    channel: responder, ..
                } = Channel::spawn(server_tls, Side::Responder);

                let id = initiator.alloc_id();
                let mut inbox = initiator.attach(id);
                responder.close(); // kill the peer: EOF on the initiator's reader

                let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .expect("in time")
                    .expect("closed event");
                assert!(matches!(event, ChannelEvent::Closed));
                assert!(
                    tokio::time::timeout(Duration::from_secs(2), inbox.recv())
                        .await
                        .expect("in time")
                        .is_none(),
                    "inbox must be released after channel death"
                );
            })
            .await;
    }
}
