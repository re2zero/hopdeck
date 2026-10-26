//! Tunnel exit side: accepts the peer's tunnel channel, authorizes it
//! against the fingerprint allowlist plus the live-session gate, then dials
//! real targets for OPENs that pass the routing rules.
//!
//! Security model (approved design): the exit **never** dials a target the
//! allow rules don't name, and a tunnel channel is only admitted while its
//! certificate fingerprint matches an authorized peer that currently holds
//! a live input session from the same IP — the tunnel rides the session's
//! trust and dies with it.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use webrtc_dtls::crypto::Certificate;

use hopdeck_ipc::IncomingPeerConfig;

use crate::client::ClientManager;

use super::channel::{Channel, ChannelEvent, ChannelParts, MAX_STREAMS_PER_CHANNEL, Side, relay};
use super::frames::{DIAL_TIMEOUT, Frame, Open, OpenAck};
use super::rules::AllowList;

/// The service's authorized-fingerprints table (std RwLock — the service
/// never holds the guard across an await).
pub type AuthorizedMap = Arc<RwLock<HashMap<String, IncomingPeerConfig>>>;

#[derive(Debug, Clone)]
pub struct ExitConfig {
    pub listen: SocketAddr,
    pub allow: AllowList,
}

/// Bind and serve the tunnel exit for the daemon's lifetime. Errors are
/// logged, never fatal to the daemon: a broken tunnel exit must not take
/// input sharing down with it.
pub fn spawn(
    config: ExitConfig,
    cert: Certificate,
    authorized: AuthorizedMap,
    client_manager: ClientManager,
) {
    tokio::task::spawn_local(async move {
        if let Err(e) = run(config, cert, authorized, client_manager).await {
            log::error!("tunnel exit stopped: {e}");
        }
    });
}

async fn run(
    config: ExitConfig,
    cert: Certificate,
    authorized: AuthorizedMap,
    client_manager: ClientManager,
) -> Result<(), std::io::Error> {
    let listener = TcpListener::bind(config.listen).await?;
    log::info!("tunnel exit listening on {}", config.listen);
    let acceptor = TlsAcceptor::from(Arc::new(
        super::tls::server_config(&cert).map_err(std::io::Error::other)?,
    ));
    loop {
        let (sock, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let authorized = authorized.clone();
        let client_manager = client_manager.clone();
        let allow = config.allow.clone();
        tokio::task::spawn_local(async move {
            handle_conn(sock, peer, acceptor, authorized, client_manager, allow).await;
        });
    }
}

async fn handle_conn(
    sock: TcpStream,
    peer: SocketAddr,
    acceptor: TlsAcceptor,
    authorized: AuthorizedMap,
    client_manager: ClientManager,
    allow: AllowList,
) {
    let tls = match acceptor.accept(sock).await {
        Ok(tls) => tls,
        Err(e) => return log::debug!("tunnel exit: tls handshake with {peer} failed: {e}"),
    };
    let Some(fingerprint) = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(super::tls::peer_fingerprint)
    else {
        return log::warn!("tunnel exit: {peer} presented no certificate");
    };
    if !authorized
        .read()
        .expect("authorized lock")
        .contains_key(&fingerprint)
    {
        return log::warn!(
            "tunnel exit: rejecting {peer}: fingerprint not authorized ({fingerprint})"
        );
    }
    // Session coupling: the tunnel rides the input session's trust. On the
    // controller the reverse session lives in the *connect* side (adopted
    // inbound transport), so liveness is the client's `alive` flag plus a
    // fingerprint match — not the listener's incoming-session table.
    let session_live = client_manager
        .get_client(peer)
        .and_then(|handle| client_manager.get_state(handle))
        .is_some_and(|(_, state)| {
            state.alive && state.peer_fingerprint.as_deref() == Some(fingerprint.as_str())
        });
    if !session_live {
        return log::warn!("tunnel exit: rejecting {peer}: no live input session for this peer");
    }
    log::info!("tunnel channel established with {peer} ({fingerprint})");

    let ChannelParts {
        channel,
        mut events,
    } = Channel::spawn(tls, Side::Responder);
    while let Some(event) = events.recv().await {
        match event {
            ChannelEvent::Open { stream, open } => {
                handle_open(&channel, stream, open, &allow).await
            }
            ChannelEvent::Closed => break,
        }
    }
    log::info!("tunnel channel with {peer} closed");
}

async fn handle_open(channel: &Channel, stream: u32, open: Open, allow: &AllowList) {
    let ack = |ok: bool, reason: &'static str| Frame::OpenAck {
        stream,
        ack: OpenAck {
            ok,
            nonce: open.nonce,
            reason: reason.to_string(),
        },
    };
    // OPENs come from the initiator, whose ids are odd; anything else is a
    // protocol violation from this side.
    if stream % 2 != 1 {
        let _ = channel.send(ack(false, "stream id out of range")).await;
        return;
    }
    if channel.is_attached(stream) {
        let _ = channel.send(ack(false, "duplicate stream id")).await;
        return;
    }
    if channel.stream_count() >= MAX_STREAMS_PER_CHANNEL {
        let _ = channel.send(ack(false, "too many open streams")).await;
        return;
    }
    if let Err(reason) = allow.check(&open.target, open.port) {
        log::info!(
            "tunnel exit: OPEN {stream} -> {}:{} rejected: {reason}",
            open.target,
            open.port
        );
        let _ = channel.send(ack(false, reason)).await;
        return;
    }
    let target = open.target.clone();
    let port = open.port;
    let dial =
        tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect((target.as_str(), port))).await;
    match dial {
        Ok(Ok(sock)) => {
            log::info!("tunnel exit: OPEN {stream} -> {target}:{port} established");
            // Attach before acknowledging so Data racing the ack lands in
            // the inbox instead of bouncing as unknown-stream.
            let inbox = channel.attach(stream);
            let _ = channel.send(ack(true, "")).await;
            relay(sock, channel.clone(), stream, inbox).await;
        }
        Ok(Err(e)) => {
            log::info!("tunnel exit: OPEN {stream} -> {target}:{port} failed: {e}");
            let _ = channel.send(ack(false, "target unreachable")).await;
        }
        Err(_) => {
            log::info!("tunnel exit: OPEN {stream} -> {target}:{port} timed out");
            let _ = channel.send(ack(false, "dial timeout")).await;
        }
    }
}
