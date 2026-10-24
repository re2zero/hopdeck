//! Tunnel forward side: listens on local ports and carries every accepted
//! connection through the channel to the exit, which dials the configured
//! target (IP or domain — the exit resolves domains).
//!
//! The channel is dialed independently of the input session and retried
//! with backoff; the *exit* refuses channels without a live input session,
//! which couples the tunnel's lifetime to the session without any
//! cross-module wiring (and makes a session drop automatically stop all
//! forwarding).

use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;

use webrtc_dtls::crypto::Certificate;

use crate::client::ClientManager;
use mousehop_ipc::ClientHandle;

use super::channel::{Channel, ChannelEvent, ChannelParts, Side, relay};
use super::exit::AuthorizedMap;
use super::frames::{DIAL_TIMEOUT, Frame, Open, OpenAck};
use super::tls::client_config;

/// One `[[clients.tunnel_forwards]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardSpec {
    pub listen: SocketAddr,
    pub target: TargetAddr,
}

/// Forward target: IP literal or hostname (resolved at the exit) + port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetAddr {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum TargetParseError {
    #[error("target must be host:port")]
    MissingPort,
    #[error("invalid port in `{0}`")]
    InvalidPort(String),
    #[error("empty host in `{0}`")]
    EmptyHost(String),
}

impl FromStr for TargetAddr {
    type Err = TargetParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix('[') {
            // Bracketed IPv6 literal: `[::1]:22`.
            let (host, port) = rest.split_once("]:").ok_or(TargetParseError::MissingPort)?;
            return Ok(TargetAddr {
                host: host.to_string(),
                port: port
                    .parse()
                    .map_err(|_| TargetParseError::InvalidPort(s.into()))?,
            });
        }
        let (host, port) = s.rsplit_once(':').ok_or(TargetParseError::MissingPort)?;
        if host.is_empty() {
            return Err(TargetParseError::EmptyHost(s.into()));
        }
        if host.contains(':') {
            // Ambiguous bare-IPv6-with-port: require the bracketed form.
            return Err(TargetParseError::EmptyHost(s.into()));
        }
        Ok(TargetAddr {
            host: host.to_string(),
            port: port
                .parse()
                .map_err(|_| TargetParseError::InvalidPort(s.into()))?,
        })
    }
}

const REDIAL_BACKOFF_MIN: Duration = Duration::from_secs(2);
const REDIAL_BACKOFF_MAX: Duration = Duration::from_secs(10);
const CLIENT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Run one forwarder for one client, for the daemon's lifetime. Public so
/// the service can spawn it; errors never take the daemon down.
pub fn spawn(
    client_manager: ClientManager,
    handle: ClientHandle,
    cert: Certificate,
    authorized: AuthorizedMap,
    forwards: Vec<ForwardSpec>,
    tunnel_port: u16,
) {
    tokio::task::spawn_local(async move {
        run(
            client_manager,
            handle,
            cert,
            authorized,
            forwards,
            tunnel_port,
        )
        .await;
    });
}

async fn run(
    client_manager: ClientManager,
    handle: ClientHandle,
    cert: Certificate,
    authorized: AuthorizedMap,
    forwards: Vec<ForwardSpec>,
    tunnel_port: u16,
) {
    // Bind every listener once for the forwarder's lifetime; rebinding on
    // each channel reconnect would flap the local ports.
    let mut listeners = Vec::new();
    for spec in forwards {
        match TcpListener::bind(spec.listen).await {
            Ok(l) => {
                log::info!(
                    "tunnel forward: listening on {} -> {}:{}",
                    spec.listen,
                    spec.target.host,
                    spec.target.port
                );
                listeners.push((l, spec));
            }
            Err(e) => log::error!(
                "tunnel forward: cannot bind {} -> {}:{}: {e}",
                spec.listen,
                spec.target.host,
                spec.target.port
            ),
        }
    }
    if listeners.is_empty() {
        return;
    }

    let current: Rc<RefCell<Option<Channel>>> = Rc::new(RefCell::new(None));
    let alive = Rc::new(Cell::new(true));
    for (listener, spec) in listeners {
        let current = current.clone();
        let alive = alive.clone();
        tokio::task::spawn_local(accept_task(listener, spec, current, alive));
    }

    let connector = TlsConnector::from(Arc::new(match client_config(&cert) {
        Ok(c) => c,
        Err(e) => return log::error!("tunnel forward: client tls config broken: {e}"),
    }));
    let mut attempts: u32 = 0;
    loop {
        let Some((_, state)) = client_manager.get_state(handle) else {
            // Client deleted: tear the forwarder (and its listeners) down.
            alive.set(false);
            if let Some(channel) = current.borrow_mut().take() {
                channel.close();
            }
            return;
        };
        if !state.active {
            tokio::time::sleep(CLIENT_POLL_INTERVAL).await;
            continue;
        }
        let Some(ip) = state
            .active_addr
            .map(|a| a.ip())
            .or_else(|| state.ips.iter().copied().next())
        else {
            tokio::time::sleep(CLIENT_POLL_INTERVAL).await;
            continue;
        };
        let addr = SocketAddr::new(ip, tunnel_port);
        match dial_channel(addr, &connector, &authorized).await {
            Ok((channel, mut events)) => {
                log::info!("tunnel channel to {addr} established");
                attempts = 0;
                *current.borrow_mut() = Some(channel);
                // Wait for channel death; the reader ignores peer OPENs on
                // this side, so `Closed` is the only event to expect.
                while let Some(event) = events.recv().await {
                    if matches!(event, ChannelEvent::Closed) {
                        break;
                    }
                }
                *current.borrow_mut() = None;
                log::info!("tunnel channel to {addr} closed; re-dialing");
            }
            Err(e) => {
                log::debug!("tunnel channel dial to {addr} failed: {e}");
            }
        }
        attempts = attempts.saturating_add(1);
        let backoff = REDIAL_BACKOFF_MIN
            .saturating_mul(attempts.min(5))
            .min(REDIAL_BACKOFF_MAX);
        tokio::time::sleep(backoff).await;
    }
}

async fn dial_channel(
    addr: SocketAddr,
    connector: &TlsConnector,
    authorized: &AuthorizedMap,
) -> Result<(Channel, tokio::sync::mpsc::Receiver<ChannelEvent>), std::io::Error> {
    let sock = TcpStream::connect(addr).await?;
    let tls = connector
        .connect(
            rustls::pki_types::ServerName::try_from("ignored").map_err(std::io::Error::other)?,
            sock,
        )
        .await?;
    let Some(fingerprint) = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(super::tls::peer_fingerprint)
    else {
        return Err(std::io::Error::other("exit presented no certificate"));
    };
    if !authorized
        .read()
        .expect("authorized lock")
        .contains_key(&fingerprint)
    {
        return Err(std::io::Error::other(format!(
            "exit fingerprint not authorized ({fingerprint})"
        )));
    }
    let ChannelParts { channel, events } = Channel::spawn(tls, Side::Initiator);
    Ok((channel, events))
}

async fn accept_task(
    listener: TcpListener,
    spec: ForwardSpec,
    current: Rc<RefCell<Option<Channel>>>,
    alive: Rc<Cell<bool>>,
) {
    loop {
        let Ok((sock, _)) = listener.accept().await else {
            return; // listener closed
        };
        if !alive.get() {
            return;
        }
        let Some(channel) = current.borrow().clone() else {
            // No channel right now: refuse the local connection instead of
            // queueing it — the app retries on its own.
            drop(sock);
            continue;
        };
        let stream = channel.alloc_id();
        let inbox = channel.attach(stream);
        tokio::task::spawn_local(pending_open(sock, channel, stream, inbox, spec.clone()));
    }
}

/// Open one stream for an accepted local connection: send OPEN, wait for
/// OPEN_ACK, then relay. On rejection / timeout the local connection is
/// dropped (the app sees a reset, which is what "target unreachable"
/// looks like to it).
async fn pending_open(
    sock: TcpStream,
    channel: Channel,
    stream: u32,
    mut inbox: tokio::sync::mpsc::Receiver<super::frames::Frame>,
    spec: ForwardSpec,
) {
    // Collision-free enough for matching one OPEN to its ACK.
    let nonce = u64::from(stream) << 56
        | std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0);
    let open = Frame::Open {
        stream,
        open: Open {
            target: spec.target.host.clone(),
            port: spec.target.port,
            nonce,
        },
    };
    if channel.send(open).await.is_err() {
        channel.detach(stream);
        return;
    }
    let ack = tokio::time::timeout(DIAL_TIMEOUT, async {
        loop {
            match inbox.recv().await {
                Some(Frame::OpenAck { stream: s, ack }) if s == stream => return Some(ack),
                // Anything else before OPEN_ACK for this stream is stale or
                // a violation; keep waiting — the timeout bounds this loop.
                Some(_) => continue,
                None => return None,
            }
        }
    })
    .await;
    match ack {
        Ok(Some(OpenAck { ok: true, .. })) => {
            relay(sock, channel, stream, inbox).await;
        }
        Ok(Some(OpenAck {
            ok: false, reason, ..
        })) => {
            log::info!(
                "tunnel forward: {}:{} rejected by exit: {reason}",
                spec.target.host,
                spec.target.port
            );
            channel.detach(stream);
        }
        Ok(None) | Err(_) => {
            channel.detach(stream);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parses_ip_and_domain() {
        let t: TargetAddr = "127.0.0.1:22".parse().expect("v4");
        assert_eq!(t.host, "127.0.0.1");
        assert_eq!(t.port, 22);

        let t: TargetAddr = "nas.local:5000".parse().expect("domain");
        assert_eq!(t.host, "nas.local");
        assert_eq!(t.port, 5000);

        let t: TargetAddr = "[::1]:8080".parse().expect("bracketed v6");
        assert_eq!(t.host, "::1");
        assert_eq!(t.port, 8080);
    }

    #[test]
    fn target_rejects_malformed() {
        assert!("no-port-here".parse::<TargetAddr>().is_err());
        assert!("host:notaport".parse::<TargetAddr>().is_err());
        assert!(":22".parse::<TargetAddr>().is_err());
        assert!(
            "::1:22".parse::<TargetAddr>().is_err(),
            "bare v6 needs brackets"
        );
        assert!("[::1]".parse::<TargetAddr>().is_err(), "missing port");
    }
}
