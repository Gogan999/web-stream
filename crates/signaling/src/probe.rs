//! Which certificate answers at an address: the first-run port test asks
//! its own public address, to tell this host apart from, say, a router's
//! admin page on the same port.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::Error;

/// The SHA-256 fingerprint (as [`crate::CertInfo`] writes it) of the
/// certificate presented at `addr`. Nothing is sent after the handshake,
/// and the certificate isn't trusted for anything: it is only compared.
pub async fn certificate_at(addr: SocketAddr, timeout: Duration) -> Result<String, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
        .with_no_client_auth();
    let connect = async {
        let tcp = TcpStream::connect(addr).await?;
        let name = ServerName::IpAddress(addr.ip().into());
        let tls = TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await?;
        let (_, conn) = tls.get_ref();
        let leaf = conn
            .peer_certificates()
            .and_then(|c| c.first())
            .ok_or_else(|| Error::Tls("no certificate".into()))?;
        Ok(crate::tls::fingerprint(leaf))
    };
    tokio::time::timeout(timeout, connect)
        .await
        .map_err(|_| Error::Io(std::io::ErrorKind::TimedOut.into()))?
}

/// Accepts any certificate, still checking that the handshake was signed
/// by its key.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_the_hosts_own_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let config = crate::SignalingConfig::new(addr, dir.path().to_path_buf());
        let (server, _events) = crate::SignalingServer::new(config).unwrap();
        let expected = server.certificate().fingerprint;
        tokio::spawn(server.serve());
        let mut got = None;
        for _ in 0..50 {
            if let Ok(f) = certificate_at(addr, Duration::from_secs(2)).await {
                got = Some(f);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(got, Some(expected));
    }

    #[tokio::test]
    async fn nothing_listening_is_an_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(certificate_at(addr, Duration::from_secs(2)).await.is_err());
    }
}
