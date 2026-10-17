use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

use crate::Error;
use crate::config::TlsSource;

const CERT_FILE: &str = "tls-cert.pem";
const KEY_FILE: &str = "tls-key.pem";
const META_FILE: &str = "tls-cert.json";
const VALIDITY: time::Duration = time::Duration::days(825);
const RENEW_BEFORE: time::Duration = time::Duration::days(30);

pub struct LoadedTls {
    pub config: Arc<ServerConfig>,
    /// SHA-256 of the leaf certificate, colon-separated hex, as browsers show it.
    pub fingerprint: String,
}

pub fn load(source: &TlsSource, data_dir: &Path) -> Result<LoadedTls, Error> {
    let (cert_pem, key_pem) = match source {
        TlsSource::Files { cert, key } => (std::fs::read(cert)?, std::fs::read(key)?),
        TlsSource::SelfSigned { hostnames } => self_signed(data_dir, hostnames)?,
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| Error::Tls(format!("certificate: {e}")))?;
    let leaf = certs
        .first()
        .ok_or_else(|| Error::Tls("no certificate in PEM".into()))?;
    let fingerprint = fingerprint(leaf);
    let key =
        PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| Error::Tls(format!("key: {e}")))?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| Error::Tls(e.to_string()))?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(LoadedTls {
        config: Arc::new(config),
        fingerprint,
    })
}

/// Reuse the stored self-signed certificate, or make a new one on first run,
/// when the host names change, or when it is within 30 days of expiry.
fn self_signed(data_dir: &Path, hostnames: &[String]) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let cert_path = data_dir.join(CERT_FILE);
    let key_path = data_dir.join(KEY_FILE);
    let meta_path = data_dir.join(META_FILE);
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    names.extend(hostnames.iter().cloned());
    let now = time::OffsetDateTime::now_utc();

    let stored: Option<CertMeta> = std::fs::read(&meta_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let current = stored
        .is_some_and(|m| m.names == names && m.not_after > (now + RENEW_BEFORE).unix_timestamp());
    if current && cert_path.exists() && key_path.exists() {
        return Ok((std::fs::read(cert_path)?, std::fs::read(key_path)?));
    }

    let mut params =
        rcgen::CertificateParams::new(names.clone()).map_err(|e| Error::Tls(e.to_string()))?;
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + VALIDITY;
    let key = rcgen::KeyPair::generate().map_err(|e| Error::Tls(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| Error::Tls(e.to_string()))?;
    let (cert, key) = (cert.pem(), key.serialize_pem());
    std::fs::create_dir_all(data_dir)?;
    crate::store::write_private(&key_path, key.as_bytes())?;
    std::fs::write(&cert_path, cert.as_bytes())?;
    let meta = CertMeta {
        names,
        not_after: params.not_after.unix_timestamp(),
    };
    std::fs::write(
        &meta_path,
        serde_json::to_vec(&meta).expect("meta serializes"),
    )?;
    tracing::info!("generated a new self-signed TLS certificate");
    Ok((cert.into_bytes(), key.into_bytes()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CertMeta {
    names: Vec<String>,
    not_after: i64,
}

fn fingerprint(cert: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(cert.as_ref());
    digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_is_stable_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let source = TlsSource::SelfSigned {
            hostnames: vec!["192.168.1.20".into()],
        };
        let first = load(&source, dir.path()).unwrap();
        let second = load(&source, dir.path()).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.fingerprint.len(), 32 * 3 - 1);

        let renamed = TlsSource::SelfSigned {
            hostnames: vec!["host.example".into()],
        };
        assert_ne!(
            load(&renamed, dir.path()).unwrap().fingerprint,
            first.fingerprint
        );
    }
}
