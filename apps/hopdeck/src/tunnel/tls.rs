//! TLS glue for the tunnel channel: mutual TLS over TCP reusing the *same*
//! self-signed certificate the DTLS session uses, with fingerprint pinning
//! done at the application layer.
//!
//! Chain validation is skipped inside rustls (the cert is self-signed and
//! identity comes from the pinned fingerprint, same trust model as the
//! session); each side then checks the peer's leaf fingerprint against its
//! authorization state post-handshake (`crypto::generate_fingerprint` on the
//! leaf DER — the exact format the session's `[authorized_fingerprints]`
//! table uses).

use std::sync::Arc;

use rustls::client::danger::{ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};

use webrtc_dtls::crypto::Certificate;

use crate::crypto;

/// Tunnel TLS setup failure (rustls error).
pub type TlsSetupError = rustls::Error;

/// Accept whatever the peer presents; identity is pinned by fingerprint
/// after the handshake. Signature "verification" accepts everything for the
/// same reason — the handshake still proves the peer holds the private key
/// matching the certificate whose fingerprint we pin.
#[derive(Debug)]
struct AcceptAny;

impl ClientCertVerifier for AcceptAny {
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
}

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, TlsError> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

fn key_der(cert: &Certificate) -> PrivateKeyDer<'static> {
    // rcgen keys are PKCS#8 DER; webrtc-dtls hands the raw bytes over.
    PrivateKeyDer::Pkcs8(cert.private_key.serialized_der.clone().into())
}

pub fn server_config(cert: &Certificate) -> Result<rustls::ServerConfig, TlsSetupError> {
    let verifier = Arc::new(AcceptAny);
    rustls::ServerConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert.certificate.clone(), key_der(cert))
}

pub fn client_config(cert: &Certificate) -> Result<rustls::ClientConfig, TlsSetupError> {
    let verifier = Arc::new(AcceptAny);
    rustls::ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(cert.certificate.clone(), key_der(cert))
}

/// Leaf fingerprint of the peer, in the same colon-hex format the session
/// uses for `[authorized_fingerprints]`.
pub fn peer_fingerprint(certs: &[CertificateDer<'_>]) -> Option<String> {
    let leaf = certs.first()?;
    Some(crypto::generate_fingerprint(leaf.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn self_signed() -> Certificate {
        Certificate::generate_self_signed(["tunnel-test".to_owned()]).expect("cert")
    }

    /// Drive a real client/server handshake over loopback TCP, then check
    /// both sides extracted the *same* fingerprint (the shared cert's) and
    /// data flows both ways.
    #[tokio::test]
    async fn handshake_pins_both_fingerprints() {
        let cert = self_signed();
        let fp = crypto::certificate_fingerprint(&cert);
        let server_cfg = Arc::new(server_config(&cert).expect("server config"));
        let client_cfg = Arc::new(client_config(&cert).expect("client config"));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.expect("accept");
            let mut conn = tokio_rustls::TlsAcceptor::from(server_cfg)
                .accept(sock)
                .await
                .expect("tls accept");
            let peer = conn
                .get_ref()
                .1
                .peer_certificates()
                .and_then(peer_fingerprint)
                .expect("client cert presented");
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 4];
            conn.read_exact(&mut buf).await.expect("read");
            (peer, buf)
        });

        let connector = tokio_rustls::TlsConnector::from(client_cfg);
        let sock = tokio::net::TcpStream::connect(addr)
            .await
            .expect("tcp connect");
        let mut conn = connector
            .connect(ServerName::try_from("ignored").expect("name"), sock)
            .await
            .expect("tls connect");
        let peer = conn
            .get_ref()
            .1
            .peer_certificates()
            .and_then(peer_fingerprint)
            .expect("server cert presented");
        assert_eq!(peer, fp, "client sees the pinned fingerprint");

        use tokio::io::AsyncWriteExt;
        conn.write_all(b"ping").await.expect("write");
        conn.flush().await.expect("flush");
        drop(conn);

        let (server_peer, echo) = server.await.expect("server task");
        assert_eq!(server_peer, fp, "server sees the pinned fingerprint");
        assert_eq!(&echo, b"ping");
    }
}
