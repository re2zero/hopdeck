//! TLS-over-TCP transport for the input session.
//!
//! UDP+DTLS is the preferred path, but a network can drop UDP in one
//! direction while passing TCP. For those networks the same session runs
//! over TLS 1.3 on a TCP stream. DTLS is a datagram protocol and must not
//! be layered on TCP, so the byte stream is adapted back into datagrams
//! here: every protocol frame travels as `u32` big-endian length + payload,
//! and the result implements [`webrtc_util::Conn`] exactly like the DTLS
//! connection. Everything above (handshake, keepalive, auth challenge,
//! reply path) is transport-agnostic and unchanged.

use std::{
    future::Future,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use hopdeck_ipc::TransportKind;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    sync::Mutex,
};
use webrtc_util::{Conn, Error, Result};

/// Frame header size: a big-endian `u32` payload length.
pub(crate) const FRAME_HEADER_LEN: usize = 4;

/// Largest payload a peer may frame. The clipboard event is the widest
/// variable-length frame the protocol defines, so this is exactly the
/// receive buffer both loops already use. A header above it — or one
/// whose length does not fit the caller's buffer — is a protocol
/// violation, not a reason to allocate: the connection is torn down.
pub(crate) const MAX_FRAME_PAYLOAD: usize = hopdeck_proto::MAX_CLIPBOARD_SIZE;

type ServerTlsStream = tokio_rustls::server::TlsStream<TcpStream>;
type ClientTlsStream = tokio_rustls::client::TlsStream<TcpStream>;

/// A [`Conn`]-shaped view of one framed TLS-over-TCP stream.
///
/// `webrtc_util::Conn` only has `&self` methods, so the read and write
/// halves sit behind async mutexes: concurrent `send`s serialize on the
/// write lock (a frame can never interleave with another), and `recv` is
/// the single reader.
pub(crate) struct TlsConn<S> {
    read: Mutex<ReadHalf<S>>,
    write: Mutex<WriteHalf<S>>,
    local: SocketAddr,
    remote: Option<SocketAddr>,
    /// Always [`TransportKind::Tcp`]; kept for diagnostics so a log line
    /// identifies the transport without the caller threading it in.
    transport: TransportKind,
    closed: AtomicBool,
}

impl<S> TlsConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Wrap an already-handshaked stream. `local` and `remote` are the
    /// underlying TCP socket addresses, captured before the stream is
    /// split (the halves own it afterwards).
    pub(crate) fn new(stream: S, local: SocketAddr, remote: Option<SocketAddr>) -> Self {
        let (read, write) = tokio::io::split(stream);
        Self {
            read: Mutex::new(read),
            write: Mutex::new(write),
            local,
            remote,
            transport: TransportKind::Tcp,
            closed: AtomicBool::new(false),
        }
    }

    async fn send_frame(&self, buf: &[u8]) -> Result<usize> {
        if buf.len() > MAX_FRAME_PAYLOAD {
            return Err(Error::Other(format!(
                "{} transport: refusing to frame {} bytes (max {MAX_FRAME_PAYLOAD})",
                self.transport,
                buf.len()
            )));
        }
        let mut write = self.write.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Other("tcp transport is closed".into()));
        }
        // Header and payload share one lock, so a concurrent sender can
        // never land between them.
        write.write_all(&(buf.len() as u32).to_be_bytes()).await?;
        write.write_all(buf).await?;
        write.flush().await?;
        Ok(buf.len())
    }

    async fn recv_frame(&self, buf: &mut [u8]) -> Result<usize> {
        let mut read = self.read.lock().await;
        let mut header = [0u8; FRAME_HEADER_LEN];
        read.read_exact(&mut header).await.map_err(peer_gone)?;
        let len = u32::from_be_bytes(header) as usize;
        if len == 0 {
            return Err(Error::Other(
                "tcp transport: peer framed an empty payload".into(),
            ));
        }
        if len > MAX_FRAME_PAYLOAD {
            log::warn!(
                "{} transport: peer framed a {len}-byte payload (max {MAX_FRAME_PAYLOAD}) — \
                 closing connection",
                self.transport
            );
            return Err(Error::Other(format!("frame too large: {len} bytes")));
        }
        if len > buf.len() {
            // The header is already consumed, so the stream can't be
            // resumed at the next frame boundary: treat it as fatal.
            return Err(Error::Other(format!(
                "frame of {len} bytes exceeds the {}-byte receive buffer",
                buf.len()
            )));
        }
        read.read_exact(&mut buf[..len]).await.map_err(peer_gone)?;
        Ok(len)
    }

    async fn close_inner(&self) -> Result<()> {
        // Idempotent: the handshake watchdog, the keepalive task and the
        // disconnect path all close the same transport.
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let mut write = self.write.lock().await;
        // Shuts the write half down (TLS close_notify). The peer sees EOF
        // and closes, which unblocks our read half.
        match write.shutdown().await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotConnected => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

impl<S> Conn for TlsConn<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // `webrtc_util::Conn` is an `#[async_trait]` trait; these are the
    // desugared signatures async-trait generates for it (`Self: Sync` for
    // `&self` receivers). Keeping them explicit avoids adding the
    // `async-trait` crate as a direct dependency just for one impl.
    fn connect<'life0, 'async_trait>(
        &'life0 self,
        _addr: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: Sync + 'async_trait,
    {
        Box::pin(async move { Err(unsupported("connect")) })
    }

    fn recv<'life0, 'life1, 'async_trait>(
        &'life0 self,
        buf: &'life1 mut [u8],
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: Sync + 'async_trait,
    {
        Box::pin(self.recv_frame(buf))
    }

    fn recv_from<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _buf: &'life1 mut [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(usize, SocketAddr)>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: Sync + 'async_trait,
    {
        Box::pin(async move { Err(unsupported("recv_from")) })
    }

    fn send<'life0, 'life1, 'async_trait>(
        &'life0 self,
        buf: &'life1 [u8],
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: Sync + 'async_trait,
    {
        Box::pin(self.send_frame(buf))
    }

    fn send_to<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _buf: &'life1 [u8],
        _target: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Result<usize>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: Sync + 'async_trait,
    {
        Box::pin(async move { Err(unsupported("send_to")) })
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.local)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote
    }

    fn close<'life0, 'async_trait>(
        &'life0 self,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: Sync + 'async_trait,
    {
        Box::pin(self.close_inner())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

fn unsupported(method: &str) -> Error {
    Error::Other(format!(
        "tcp transport: `{method}` is not supported on a connection-oriented stream"
    ))
}

/// Map a read failure onto a `Conn` error. EOF must never become
/// `Ok(0)`: both receive loops treat a zero-length read as "nothing to
/// do" and would spin at 100% CPU.
fn peer_gone(e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::UnexpectedEof {
        Error::Other("tcp transport: peer closed the connection".into())
    } else {
        e.into()
    }
}

/// Wrap a server-side (accepted) TLS stream.
pub(crate) fn server_conn(stream: ServerTlsStream) -> io::Result<TlsConn<ServerTlsStream>> {
    let (local, remote) = socket_addrs(stream.get_ref().0)?;
    Ok(TlsConn::new(stream, local, remote))
}

/// Wrap a client-side (dialed) TLS stream.
pub(crate) fn client_conn(stream: ClientTlsStream) -> io::Result<TlsConn<ClientTlsStream>> {
    let (local, remote) = socket_addrs(stream.get_ref().0)?;
    Ok(TlsConn::new(stream, local, remote))
}

fn socket_addrs(tcp: &TcpStream) -> io::Result<(SocketAddr, Option<SocketAddr>)> {
    Ok((tcp.local_addr()?, tcp.peer_addr().ok()))
}

/// Shared `Arc<dyn Conn>` view, the form the session layer stores.
pub(crate) fn into_conn<S>(conn: TlsConn<S>) -> Arc<dyn Conn + Send + Sync>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Arc::new(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    fn addr() -> SocketAddr {
        "127.0.0.1:4252".parse().expect("test address")
    }

    /// One framed view plus the raw peer end, so tests can write
    /// arbitrary (including malformed) bytes.
    fn pair(capacity: usize) -> (Arc<TlsConn<DuplexStream>>, DuplexStream) {
        let (ours, peer) = tokio::io::duplex(capacity);
        let conn = Arc::new(TlsConn::new(ours, addr(), Some(addr())));
        (conn, peer)
    }

    async fn read_frame(peer: &mut DuplexStream) -> Vec<u8> {
        let mut header = [0u8; FRAME_HEADER_LEN];
        peer.read_exact(&mut header).await.expect("read header");
        let len = u32::from_be_bytes(header) as usize;
        let mut payload = vec![0u8; len];
        peer.read_exact(&mut payload).await.expect("read payload");
        payload
    }

    #[tokio::test]
    async fn recv_assembles_a_frame_split_across_writes() {
        let (conn, mut peer) = pair(1024);
        let payload = b"hello-from-a-split-frame";

        peer.write_all(&(payload.len() as u32).to_be_bytes()[..2])
            .await
            .expect("partial header");
        peer.flush().await.expect("flush");
        tokio::task::yield_now().await;

        peer.write_all(&(payload.len() as u32).to_be_bytes()[2..])
            .await
            .expect("rest of header");
        peer.write_all(&payload[..5])
            .await
            .expect("partial payload");
        peer.flush().await.expect("flush");
        tokio::task::yield_now().await;

        peer.write_all(&payload[5..])
            .await
            .expect("rest of payload");
        peer.flush().await.expect("flush");

        let mut buf = [0u8; MAX_FRAME_PAYLOAD];
        let n = conn.recv(&mut buf).await.expect("recv frame");
        assert_eq!(&buf[..n], payload);
    }

    #[tokio::test]
    async fn recv_returns_one_frame_at_a_time_for_coalesced_frames() {
        let (conn, mut peer) = pair(1024);
        let mut both = Vec::new();
        for payload in [b"first".as_slice(), b"second".as_slice()] {
            both.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            both.extend_from_slice(payload);
        }
        // A single write that carries two complete frames (TCP is a byte
        // stream, so the receiver must not merge them).
        peer.write_all(&both).await.expect("write coalesced");
        peer.flush().await.expect("flush");

        let mut buf = [0u8; MAX_FRAME_PAYLOAD];
        let n = conn.recv(&mut buf).await.expect("first frame");
        assert_eq!(&buf[..n], b"first");
        let n = conn.recv(&mut buf).await.expect("second frame");
        assert_eq!(&buf[..n], b"second");
    }

    #[tokio::test]
    async fn oversize_frame_is_rejected_without_reading_payload() {
        let (conn, mut peer) = pair(1024);
        peer.write_all(&(MAX_FRAME_PAYLOAD as u32 + 1).to_be_bytes())
            .await
            .expect("write oversize header");
        peer.flush().await.expect("flush");

        let mut buf = [0u8; MAX_FRAME_PAYLOAD];
        let err = conn.recv(&mut buf).await.expect_err("oversize rejected");
        assert!(
            format!("{err}").contains("frame too large"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn eof_surfaces_as_error_never_ok_zero() {
        let (conn, peer) = pair(64);
        drop(peer);
        let mut buf = [0u8; MAX_FRAME_PAYLOAD];
        let err = conn.recv(&mut buf).await.expect_err("EOF is an error");
        assert!(
            format!("{err}").contains("closed"),
            "unexpected error: {err}"
        );

        // A truncated header is the same failure mode (the peer vanished
        // between the two writes of a real frame).
        let (conn, mut peer) = pair(64);
        peer.write_all(&[0u8, 0]).await.expect("partial header");
        peer.flush().await.expect("flush");
        peer.shutdown().await.expect("shutdown peer");
        let err = conn
            .recv(&mut buf)
            .await
            .expect_err("partial header is an error");
        assert!(
            format!("{err}").contains("closed"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn zero_length_frame_is_rejected() {
        let (conn, mut peer) = pair(64);
        peer.write_all(&0u32.to_be_bytes())
            .await
            .expect("write empty header");
        peer.flush().await.expect("flush");
        let mut buf = [0u8; MAX_FRAME_PAYLOAD];
        assert!(conn.recv(&mut buf).await.is_err());
    }

    #[tokio::test]
    async fn concurrent_sends_never_interleave_frames() {
        const SENDERS: usize = 32;
        const PAYLOAD: usize = 24;

        // Small capacity so the writers must block on the reader rather
        // than buffering the whole burst up front.
        let (conn, mut peer) = pair(64);
        let reader = tokio::spawn(async move {
            let mut frames = Vec::with_capacity(SENDERS);
            for _ in 0..SENDERS {
                frames.push(read_frame(&mut peer).await);
            }
            frames
        });

        let mut tasks = Vec::with_capacity(SENDERS);
        for i in 0..SENDERS {
            let conn = conn.clone();
            tasks.push(tokio::spawn(async move {
                let payload = vec![i as u8; PAYLOAD];
                let sent = conn.send(&payload).await.expect("send frame");
                assert_eq!(sent, PAYLOAD);
            }));
        }
        for task in tasks {
            task.await.expect("sender task");
        }
        let frames = reader.await.expect("reader task");

        assert_eq!(frames.len(), SENDERS);
        let mut seen: Vec<u8> = frames.iter().map(|frame| frame[0]).collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            (0..SENDERS as u8).collect::<Vec<_>>(),
            "every frame arrived exactly once"
        );
        for frame in &frames {
            assert_eq!(frame.len(), PAYLOAD, "frame length preserved");
            assert!(
                frame.iter().all(|byte| *byte == frame[0]),
                "frame bytes stayed contiguous: two sends interleaved"
            );
        }
    }

    #[tokio::test]
    async fn send_after_close_fails_and_close_is_idempotent() {
        let (conn, mut peer) = pair(64);
        let drain = tokio::spawn(async move {
            let mut sink = [0u8; 64];
            let _ = peer.read(&mut sink).await;
        });

        conn.close().await.expect("first close");
        conn.close().await.expect("second close is a no-op");
        assert!(conn.send(b"x").await.is_err());
        drop(drain);
    }

    #[tokio::test]
    async fn addresses_are_reported_from_the_wrapped_stream() {
        let (conn, _peer) = pair(64);
        assert_eq!(conn.local_addr().expect("local addr"), addr());
        assert_eq!(conn.remote_addr(), Some(addr()));
        assert!(conn.as_any().is::<TlsConn<DuplexStream>>());
    }
}
