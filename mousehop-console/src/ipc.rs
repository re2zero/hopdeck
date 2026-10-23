//! Daemon connection for the console.
//!
//! A background task connects to the mousehop service socket, hands the
//! request writer to the [`Console`], and pumps daemon broadcasts through a
//! channel so the blocking reader never runs on the UI thread. The reader
//! thread exits when the console (channel receiver) goes away or the daemon
//! closes the stream.

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::{App, WeakEntity};
use mousehop_ipc::{FrontendEvent, FrontendRequest, FrontendRequestWriter};
use std::sync::{Arc, Mutex};

use crate::console::Console;

/// Line writer shared between UI event handlers. Requests are single
/// line writes, so a mutex is enough to serialize concurrent senders.
#[derive(Clone)]
pub struct RequestWriter {
    inner: Arc<Mutex<FrontendRequestWriter>>,
}

impl RequestWriter {
    fn new(inner: FrontendRequestWriter) -> Self {
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn send(&self, request: FrontendRequest) {
        if let Ok(mut writer) = self.inner.lock() {
            if let Err(e) = writer.request(request) {
                log::warn!("ipc request failed: {e}");
            }
        }
    }
}

/// Connection lifecycle plus daemon broadcasts, delivered to the console
/// on the UI executor.
pub enum FeedEvent {
    Connected(RequestWriter),
    /// `Some(reason)` when connecting never succeeded, `None` when an
    /// established connection was lost.
    Disconnected(Option<String>),
    Daemon(Box<FrontendEvent>),
}

pub fn spawn(cx: &mut App, console: WeakEntity<Console>) {
    cx.spawn(async move |cx| {
        let connected = cx
            .background_executor()
            .spawn(async { mousehop_ipc::connect_once() })
            .await;
        let (reader, writer) = match connected {
            Ok(pair) => pair,
            Err(e) => {
                log::warn!("ipc connect failed: {e}");
                let _ = console.update(cx, |console, cx| {
                    console.on_feed(FeedEvent::Disconnected(Some(e.to_string())), cx)
                });
                return;
            }
        };

        let (feed_tx, mut feed_rx) = unbounded::<FeedEvent>();
        spawn_reader(reader, feed_tx.clone());

        if console
            .update(cx, |console, cx| {
                console.on_feed(FeedEvent::Connected(RequestWriter::new(writer)), cx)
            })
            .is_err()
        {
            return;
        }

        while let Some(event) = feed_rx.next().await {
            if console
                .update(cx, |console, cx| console.on_feed(event, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

/// Blocking read loop on its own thread; `next_event()` returning `None`
/// means the daemon hung up.
fn spawn_reader(
    mut reader: mousehop_ipc::FrontendEventReader,
    feed_tx: UnboundedSender<FeedEvent>,
) {
    std::thread::spawn(move || {
        loop {
            match reader.next_event() {
                Some(Ok(event)) => {
                    if feed_tx
                        .unbounded_send(FeedEvent::Daemon(Box::new(event)))
                        .is_err()
                    {
                        break;
                    }
                }
                // A malformed line must not tear down the feed.
                Some(Err(e)) => log::warn!("ipc read error: {e}"),
                None => break,
            }
        }
        let _ = feed_tx.unbounded_send(FeedEvent::Disconnected(None));
    });
}
