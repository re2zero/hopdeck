//! Daemon connection for the console.
//!
//! A background task connects to the mousehop service socket, hands the
//! request writer to the [`Console`], and pumps daemon broadcasts through a
//! channel so the blocking reader never runs on the UI thread. Both a
//! failed connect and a lost connection retry until the daemon is back, so
//! a daemon restart heals without restarting the console.

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui_kit::{App, WeakEntity};
use mousehop_ipc::{FrontendEvent, FrontendRequest, FrontendRequestWriter};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::console::Console;

/// Pause between reconnect attempts.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

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
    /// `Some(reason)` marks a failed connect (or reconnect) attempt,
    /// `None` a lost established connection.
    Disconnected(Option<String>),
    Daemon(Box<FrontendEvent>),
}

pub fn spawn(cx: &mut App, console: WeakEntity<Console>) {
    cx.spawn(async move |cx| {
        // Each failure streak is reported once; the retries themselves
        // stay silent so an outage doesn't spam the journal every delay.
        let mut reported_problem = false;
        loop {
            let connected = cx
                .background_executor()
                .spawn(async { mousehop_ipc::connect_once() })
                .await;
            let (reader, writer) = match connected {
                Ok(pair) => {
                    reported_problem = false;
                    pair
                }
                Err(e) => {
                    if !reported_problem {
                        log::warn!("ipc connect failed: {e}");
                        reported_problem = true;
                        let delivered = console
                            .update(cx, |console, cx| {
                                console.on_feed(FeedEvent::Disconnected(Some(e.to_string())), cx)
                            })
                            .is_ok();
                        if !delivered {
                            return;
                        }
                    } else {
                        log::debug!("ipc connect failed: {e}");
                    }
                    cx.background_executor().timer(RECONNECT_DELAY).await;
                    continue;
                }
            };

            let (feed_tx, mut feed_rx) = unbounded::<FeedEvent>();
            // The reader thread is the channel's only sender: when the
            // daemon hangs up it delivers `Disconnected(None)` and drops
            // the sender, so the pump below drains and we can re-dial.
            spawn_reader(reader, feed_tx);

            if console
                .update(cx, |console, cx| {
                    console.on_feed(FeedEvent::Connected(RequestWriter::new(writer)), cx)
                })
                .is_err()
            {
                return;
            }

            while let Some(event) = feed_rx.next().await {
                let hung_up = matches!(event, FeedEvent::Disconnected(_));
                if console
                    .update(cx, |console, cx| console.on_feed(event, cx))
                    .is_err()
                {
                    return;
                }
                if hung_up {
                    break;
                }
            }

            // Session state was cleared by `on_feed`; wait, then re-dial.
            cx.background_executor().timer(RECONNECT_DELAY).await;
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
