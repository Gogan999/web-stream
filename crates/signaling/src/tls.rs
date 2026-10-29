use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
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
    /// The certificate in use.
    pub certs: Arc<CertStore>,
}

pub fn load(source: &TlsSource, data_dir: &Path) -> Result<LoadedTls, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let store = |(cert, key): (Vec<u8>, Vec<u8>)| CertStore::new(&provider, &cert, &key);
    let certs = match source {
        TlsSource::Files { cert, key } => store((std::fs::read(cert)?, std::fs::read(key)?))?,
        TlsSource::SelfSigned { hostnames } => store(self_signed(data_dir, hostnames)?)?,
    };
    let certs = Arc::new(certs);
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Tls(e.to_string()))?
        .with_no_client_auth()
        .with_cert_resolver(certs.clone());
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(LoadedTls {
        config: Arc::new(config),
        certs,
    })
}

/// The certificate the server presents.
#[derive(Debug)]
pub struct CertStore {
    current: Current,
}

#[derive(Debug)]
struct Current {
    key: Arc<CertifiedKey>,
    info: CertInfo,
}

/// What the host shows about a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertInfo {
    /// SHA-256 of the leaf certificate, colon-separated hex, as browsers show it.
    pub fingerprint: String,
    /// Validity, in Unix seconds.
    pub not_before: i64,
    pub not_after: i64,
    /// The issuer is the subject: browsers warn until the user accepts it.
    pub self_signed: bool,
}

impl CertStore {
    fn new(provider: &CryptoProvider, cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            current: parse(provider, cert_pem, key_pem)?,
        })
    }

    pub fn info(&self) -> CertInfo {
        self.current.info.clone()
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.key.clone())
    }
}

fn parse(provider: &CryptoProvider, cert_pem: &[u8], key_pem: &[u8]) -> Result<Current, Error> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| Error::Tls(format!("certificate: {e}")))?;
    let leaf = certs
        .first()
        .ok_or_else(|| Error::Tls("no certificate in PEM".into()))?;
    let info = cert_info(leaf)?;
    let key =
        PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| Error::Tls(format!("key: {e}")))?;
    let key = provider
        .key_provider
        .load_private_key(key)
        .map_err(|e| Error::Tls(format!("key: {e}")))?;
    let key = CertifiedKey::new(certs, key);
    key.keys_match()
        .map_err(|e| Error::Tls(format!("the key is not the certificate's: {e}")))?;
    Ok(Current {
        key: Arc::new(key),
        info,
    })
}

pub(crate) fn cert_info(leaf: &CertificateDer<'_>) -> Result<CertInfo, Error> {
    let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref())
        .map_err(|e| Error::Tls(format!("certificate: {e}")))?;
    let validity = cert.validity();
    Ok(CertInfo {
        fingerprint: fingerprint(leaf),
        not_before: validity.not_before.timestamp(),
        not_after: validity.not_after.timestamp(),
        self_signed: cert.issuer() == cert.subject(),
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

pub(crate) fn fingerprint(cert: &CertificateDer<'_>) -> String {
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
        let first = load(&source, dir.path()).unwrap().certs.info();
        let second = load(&source, dir.path()).unwrap().certs.info();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.fingerprint.len(), 32 * 3 - 1);
        assert!(first.self_signed);

        let renamed = TlsSource::SelfSigned {
            hostnames: vec!["host.example".into()],
        };
        assert_ne!(
            load(&renamed, dir.path()).unwrap().certs.info().fingerprint,
            first.fingerprint
        );
    }
}
