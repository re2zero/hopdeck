//! Local side channel that streams decoded video frames from the daemon
//! to the frontend process (M3a increment ④).
//!
//! The main frontend channel is line-delimited JSON — structurally wrong
//! for video: a `Vec<u8>` payload serializes as a JSON number array and
//! the design's "video never competes with input" rule (D5) applies to
//! the frontend feed just as much as to the network channel. This module
//! therefore rides a sibling Unix socket with fixed binary framing and a
//! strict one-way flow: the daemon pushes, the frontend only reads.
//!
//! Frame record (big-endian, proto convention):
//!
//! ```text
//! [width u16][height u16][index u64][len u32][bgra bytes]
//! ```
//!
//! `len` is always `width * height * 4` (the pipeline's canonical tight
//! BGRA8, `frame.rs`); it is framed explicitly so a future stride or
//! format bump can renegotiate without breaking older readers, which
//! reject any mismatched record as malformed. Frontends render the
//! newest frame and drop everything older — the daemon drops the frame
//! outright when a slow frontend reader cannot keep up (latest-wins, no
//! backlog).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::channel::mpsc;

/// Byte length of a frame record's fixed header.
pub const FRAME_HEADER_LEN: usize = 16;
/// Upper bound for one record's payload. The protocol is local-only, but
/// a daemon/frontend version mismatch must fail cleanly instead of
/// allocating from a bogus length; this admits 8K² BGRA with headroom.
pub const FRAME_PAYLOAD_CAP: usize = 256 * 1024 * 1024;

/// A decoded frame as delivered to the frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFeedFrame {
    pub width: u16,
    pub height: u16,
    /// Producer-side frame counter (0 when the source carries none).
    pub index: u64,
    /// Tight BGRA8 pixels, `width * height * 4` bytes.
    pub bgra: Vec<u8>,
}

/// Encode one frame record (header + payload).
pub fn encode_frame(width: u16, height: u16, index: u64, bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + bgra.len());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(&(bgra.len() as u32).to_be_bytes());
    out.extend_from_slice(bgra);
    out
}

/// Parse a frame record from a complete header + payload byte slice.
pub fn decode_frame(bytes: &[u8]) -> Result<VideoFeedFrame, std::io::Error> {
    use std::io::{Error, ErrorKind};
    if bytes.len() < FRAME_HEADER_LEN {
        return Err(Error::new(ErrorKind::InvalidData, "truncated frame header"));
    }
    let width = u16::from_be_bytes([bytes[0], bytes[1]]);
    let height = u16::from_be_bytes([bytes[2], bytes[3]]);
    let index = u64::from_be_bytes(bytes[4..12].try_into().expect("fixed-size slice"));
    let len = u32::from_be_bytes(bytes[12..16].try_into().expect("fixed-size slice")) as usize;
    let expected = usize::from(width) * usize::from(height) * 4;
    if width == 0 || height == 0 {
        return Err(Error::new(ErrorKind::InvalidData, "zero-sized frame"));
    }
    if len != expected || len > FRAME_PAYLOAD_CAP {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!("frame payload {len} != {expected} or over cap"),
        ));
    }
    Ok(VideoFeedFrame {
        width,
        height,
        index,
        bgra: bytes[FRAME_HEADER_LEN..].to_vec(),
    })
}

/// The path of the local video feed socket, next to the main frontend
/// socket in the same runtime directory.
#[cfg(unix)]
pub fn video_socket_path() -> Result<std::path::PathBuf, crate::SocketPathError> {
    const VIDEO_SOCKET_NAME: &str = "mousehop-video.sock";
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let xdg_runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .map_err(crate::SocketPathError::XdgRuntimeDirNotFound)?;
        Ok(std::path::Path::new(xdg_runtime_dir.as_str()).join(VIDEO_SOCKET_NAME))
    }
    #[cfg(all(unix, target_os = "macos"))]
    {
        let home = std::env::var("HOME").map_err(crate::SocketPathError::HomeDirNotFound)?;
        Ok(std::path::Path::new(home.as_str())
            .join("Library")
            .join("Caches")
            .join(VIDEO_SOCKET_NAME))
    }
}

// --------------------------------------------------------------------- //
// Daemon side: the feed hub                                              //
// --------------------------------------------------------------------- //

/// Daemon-side handle for pushing decoded frames to every connected
/// frontend reader. Cheap to clone; `push` with no readers is a no-op.
#[derive(Clone, Default)]
pub struct VideoFeedHandle {
    #[cfg(unix)]
    inner: Arc<Hub>,
    #[cfg(not(unix))]
    _private: (),
}

#[cfg(unix)]
#[derive(Default)]
struct Hub {
    next_id: AtomicU64,
    /// One bounded queue per connected frontend. `try_send` keeps the
    /// daemon lock-free against a stalled reader: a full queue drops the
    /// frame (latest-wins) instead of backpressuring the decoder.
    clients: Mutex<HashMap<u64, mpsc::Sender<Vec<u8>>>>,
}

impl VideoFeedHandle {
    /// An inert handle; call [`VideoFeedHandle::serve`] once to accept
    /// frontend readers.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push one decoded frame to every connected frontend. Frames are
    /// dropped when no reader is attached or a reader's queue is full —
    /// the frontend always renders the newest picture it has anyway.
    pub fn push(&self, width: u16, height: u16, index: u64, bgra: &[u8]) {
        #[cfg(unix)]
        {
            if bgra.is_empty() {
                return;
            }
            let record = encode_frame(width, height, index, bgra);
            let mut clients = self.inner.clients.lock().expect("video feed clients lock");
            let mut stale = Vec::new();
            for (id, sender) in clients.iter_mut() {
                // Full queues drop the frame silently (latest-wins);
                // disconnected senders prune the client entry.
                if let Err(e) = sender.try_send(record.clone()) {
                    if e.is_disconnected() {
                        stale.push(*id);
                    }
                }
            }
            for id in stale {
                clients.remove(&id);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (width, height, index, bgra);
        }
    }

    /// Bind the local feed socket and stream pushed frames to every
    /// frontend that connects. Runs until the process exits; a bind
    /// failure logs and disables the feed (never fatal to the daemon).
    #[cfg(unix)]
    pub async fn serve(self) {
        let socket_path = match video_socket_path() {
            Ok(path) => path,
            Err(e) => {
                log::warn!("video feed: no socket path ({e}) — frontend preview disabled");
                return;
            }
        };
        if socket_path.exists() {
            let _ = std::fs::remove_file(&socket_path);
        }
        let listener = match tokio::net::UnixListener::bind(&socket_path) {
            Ok(listener) => listener,
            Err(e) => {
                log::warn!("video feed: bind {socket_path:?} failed: {e} — preview disabled");
                return;
            }
        };
        log::info!("video feed: serving frames on {socket_path:?}");
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => self.adopt(stream),
                Err(e) => log::warn!("video feed: accept failed: {e}"),
            }
        }
    }

    /// Wire one frontend connection: a bounded queue accepts frames from
    /// `push`, and a writer task drains it into the socket.
    #[cfg(unix)]
    fn adopt(&self, stream: tokio::net::UnixStream) {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(2);
        let (socket_rx, mut socket_tx) = tokio::io::split(stream);
        self.inner
            .clients
            .lock()
            .expect("video feed clients lock")
            .insert(id, tx);
        log::debug!("video feed: frontend #{id} connected");
        tokio::task::spawn_local(async move {
            // The protocol is one-way, but the reader half must stay
            // alive or the peer sees EOF immediately — it lives (and
            // closes) with this task.
            let _socket_rx = socket_rx;
            while let Some(record) = rx.next().await {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = socket_tx.write_all(&record).await {
                    log::debug!("video feed: frontend #{id} write failed: {e}");
                    break;
                }
            }
            log::debug!("video feed: frontend #{id} disconnected");
        });
    }
}

// --------------------------------------------------------------------- //
// Frontend side: connect + framed reads                                  //
// --------------------------------------------------------------------- //

/// Connect to the daemon's video feed socket.
#[cfg(unix)]
pub fn connect_video_feed() -> Result<std::os::unix::net::UnixStream, std::io::Error> {
    let path = video_socket_path()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;
    std::os::unix::net::UnixStream::connect(path)
}

/// Blocking read of exactly one frame record. Pairs with a read timeout
/// on the stream so callers can interleave shutdown checks.
#[cfg(unix)]
pub fn read_video_frame(
    stream: &mut std::os::unix::net::UnixStream,
) -> Result<VideoFeedFrame, std::io::Error> {
    use std::io::{Error, ErrorKind, Read};
    let mut header = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut header)?;
    let width = u16::from_be_bytes([header[0], header[1]]);
    let height = u16::from_be_bytes([header[2], header[3]]);
    let len = u32::from_be_bytes(header[12..16].try_into().expect("fixed-size slice")) as usize;
    let expected = usize::from(width) * usize::from(height) * 4;
    if width == 0 || height == 0 || len != expected || len > FRAME_PAYLOAD_CAP {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "malformed video frame header",
        ));
    }
    let mut bgra = vec![0u8; len];
    stream.read_exact(&mut bgra)?;
    Ok(VideoFeedFrame {
        width,
        height,
        index: u64::from_be_bytes(header[4..12].try_into().expect("fixed-size slice")),
        bgra,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let bgra = vec![7u8; 3 * 2 * 4];
        let record = encode_frame(3, 2, 42, &bgra);
        let frame = decode_frame(&record).expect("round trip");
        assert_eq!(frame.width, 3);
        assert_eq!(frame.height, 2);
        assert_eq!(frame.index, 42);
        assert_eq!(frame.bgra, bgra);
    }

    #[test]
    fn decode_rejects_mismatched_and_zero_sizes() {
        assert!(decode_frame(&encode_frame(0, 2, 0, &[])).is_err());
        assert!(decode_frame(&encode_frame(2, 0, 0, &[])).is_err());
        // A 2×1 frame announcing 8 payload bytes is fine; announcing 7
        // (wrong length) or 0 must be rejected.
        let mut record = encode_frame(2, 1, 0, &[0u8; 8]);
        assert!(decode_frame(&record).is_ok());
        let len_pos = 12;
        record[len_pos..16].copy_from_slice(&7u32.to_be_bytes());
        assert!(decode_frame(&record).is_err());
        record[len_pos..16].copy_from_slice(&0u32.to_be_bytes());
        assert!(decode_frame(&record).is_err());
        assert!(decode_frame(&record[..FRAME_HEADER_LEN - 1]).is_err());
    }

    /// The hub runs on `spawn_local`, so the test drives it from a
    /// `LocalSet` on the current-thread runtime. The client side does
    /// *blocking* reads, which must never happen on the executor thread
    /// (it would starve the hub's writer task — the exact deadlock this
    /// test caught on its first run), so it runs on its own thread and
    /// reports via a oneshot.
    #[test]
    fn hub_delivers_frames_to_connected_frontend() {
        use std::io::Read;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let handle = VideoFeedHandle::new();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "mousehop-video-feed-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("tmp dir");
            // Serve on a test-specific path by pointing the runtime dir at it.
            std::env::set_var("XDG_RUNTIME_DIR", &dir);
            tokio::task::spawn_local(handle.clone().serve());

            let (result_tx, result_rx) = futures::channel::oneshot::channel();
            std::thread::spawn(move || {
                let mut client = loop {
                    match crate::video_feed::connect_video_feed() {
                        Ok(stream) => break stream,
                        Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
                    }
                };
                let mut buf = vec![0u8; FRAME_HEADER_LEN + 16];
                client.read_exact(&mut buf).expect("frame bytes");
                let first = decode_frame(&buf).expect("decoded");
                client.read_exact(&mut buf).expect("second frame");
                let second_index = decode_frame(&buf).expect("decoded").index;
                let _ = result_tx.send((first, second_index));
            });

            // Push like a decoder would — continuously, until the client
            // reports two records. A push racing the client's adopt lands
            // on zero readers and is dropped (latest-wins by design), so
            // a single push is not observable; consecutive pushes are.
            let pusher = handle;
            tokio::task::spawn_local(async move {
                let mut index = 9u64;
                loop {
                    pusher.push(2, 2, index, &[1u8; 16]);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    index += 1;
                }
            });
            let (first, second_index) = result_rx.await.expect("client thread finished");
            assert_eq!(first.width, 2);
            assert_eq!(first.bgra, vec![1u8; 16]);
            // Records are queued in push order, so the two reads observe
            // consecutive indexes.
            assert!(first.index >= 9);
            assert_eq!(second_index, first.index + 1);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
