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
const CA_CERT_FILE: &str = "tls-ca-cert.pem";
const CA_KEY_FILE: &str = "tls-ca-key.pem";
const CA_META_FILE: &str = "tls-ca.json";
const VALIDITY: time::Duration = time::Duration::days(825);
const CA_VALIDITY: time::Duration = time::Duration::days(3650);
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
        TlsSource::SelfSigned { hostnames } => {
            let ca = local_ca(data_dir)?;
            let mut certs = store(leaf(data_dir, hostnames, &ca)?)?;
            certs.current.info.self_signed = true;
            certs.ca = Some(ca.der);
            certs
        }
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
    /// The host's own CA certificate (DER), when it issued the server certificate.
    ca: Option<Vec<u8>>,
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
    /// Issued by the host itself: browsers warn until its CA is trusted.
    pub self_signed: bool,
}

impl CertStore {
    fn new(provider: &CryptoProvider, cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            current: parse(provider, cert_pem, key_pem)?,
            ca: None,
        })
    }

    pub fn info(&self) -> CertInfo {
        self.current.info.clone()
    }

    /// The host CA certificate in DER, for devices to install and trust.
    pub fn ca_der(&self) -> Option<&[u8]> {
        self.ca.as_deref()
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

/// The host's certificate authority, created once and kept across server certificate renewals.
struct LocalCa {
    params: rcgen::CertificateParams,
    key: rcgen::KeyPair,
    pem: String,
    der: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CaMeta {
    name: String,
    not_before: i64,
    not_after: i64,
}

fn ca_params(meta: &CaMeta) -> Result<rcgen::CertificateParams, Error> {
    let mut params = rcgen::CertificateParams::default();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, meta.name.clone());
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "web-stream");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    params.not_before = time::OffsetDateTime::from_unix_timestamp(meta.not_before)
        .map_err(|e| Error::Tls(e.to_string()))?;
    params.not_after = time::OffsetDateTime::from_unix_timestamp(meta.not_after)
        .map_err(|e| Error::Tls(e.to_string()))?;
    Ok(params)
}

/// Loads the host CA, or creates it on first run or near its expiry.
fn local_ca(data_dir: &Path) -> Result<LocalCa, Error> {
    let cert_path = data_dir.join(CA_CERT_FILE);
    let key_path = data_dir.join(CA_KEY_FILE);
    let meta_path = data_dir.join(CA_META_FILE);
    let now = time::OffsetDateTime::now_utc();
    let stored: Option<CaMeta> = std::fs::read(&meta_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    if let Some(meta) = stored.filter(|m| m.not_after > (now + VALIDITY).unix_timestamp()) {
        if let (Ok(pem), Ok(key)) = (
            std::fs::read_to_string(&cert_path),
            std::fs::read_to_string(&key_path),
        ) {
            let key = rcgen::KeyPair::from_pem(&key).map_err(|e| Error::Tls(e.to_string()))?;
            let der = CertificateDer::from_pem_slice(pem.as_bytes())
                .map_err(|e| Error::Tls(format!("CA certificate: {e}")))?
                .to_vec();
            return Ok(LocalCa {
                params: ca_params(&meta)?,
                key,
                pem,
                der,
            });
        }
    }

    let mut id = [0u8; 4];
    getrandom::fill(&mut id).map_err(|e| Error::Tls(e.to_string()))?;
    let meta = CaMeta {
        name: format!(
            "web-stream host CA {}",
            id.iter().map(|b| format!("{b:02X}")).collect::<String>()
        ),
        not_before: (now - time::Duration::days(1)).unix_timestamp(),
        not_after: (now + CA_VALIDITY).unix_timestamp(),
    };
    let params = ca_params(&meta)?;
    let key = rcgen::KeyPair::generate().map_err(|e| Error::Tls(e.to_string()))?;
    let cert = params
        .self_signed(&key)
        .map_err(|e| Error::Tls(e.to_string()))?;
    let pem = cert.pem();
    std::fs::create_dir_all(data_dir)?;
    crate::store::write_private(&key_path, key.serialize_pem().as_bytes())?;
    std::fs::write(&cert_path, pem.as_bytes())?;
    std::fs::write(
        &meta_path,
        serde_json::to_vec(&meta).expect("meta serializes"),
    )?;
    // A new CA invalidates any server certificate issued by an earlier one.
    let _ = std::fs::remove_file(data_dir.join(META_FILE));
    tracing::info!("generated a new host certificate authority");
    Ok(LocalCa {
        params,
        key,
        pem,
        der: cert.der().to_vec(),
    })
}

/// The server certificate and its chain, issued by the host CA. Reused until the host names
/// change or it is within 30 days of expiry.
fn leaf(data_dir: &Path, hostnames: &[String], ca: &LocalCa) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let cert_path = data_dir.join(CERT_FILE);
    let key_path = data_dir.join(KEY_FILE);
    let meta_path = data_dir.join(META_FILE);
    let mut names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
    names.extend(hostnames.iter().cloned());
    let now = time::OffsetDateTime::now_utc();

    let stored: Option<CertMeta> = std::fs::read(&meta_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let current = stored.is_some_and(|m| {
        m.names == names
            && m.not_after > (now + RENEW_BEFORE).unix_timestamp()
            && m.ca.as_deref() == Some(ca.pem.as_str())
    });
    if current && cert_path.exists() && key_path.exists() {
        return Ok((std::fs::read(cert_path)?, std::fs::read(key_path)?));
    }

    let mut params =
        rcgen::CertificateParams::new(names.clone()).map_err(|e| Error::Tls(e.to_string()))?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "web-stream host");
    params.not_before = now - time::Duration::days(1);
    params.not_after = now + VALIDITY;
    params.is_ca = rcgen::IsCa::ExplicitNoCa;
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    let key = rcgen::KeyPair::generate().map_err(|e| Error::Tls(e.to_string()))?;
    let issuer = rcgen::Issuer::from_params(&ca.params, &ca.key);
    let cert = params
        .signed_by(&key, &issuer)
        .map_err(|e| Error::Tls(e.to_string()))?;
    let chain = format!("{}{}", cert.pem(), ca.pem);
    let key = key.serialize_pem();
    std::fs::create_dir_all(data_dir)?;
    crate::store::write_private(&key_path, key.as_bytes())?;
    std::fs::write(&cert_path, chain.as_bytes())?;
    let meta = CertMeta {
        names,
        not_after: params.not_after.unix_timestamp(),
        ca: Some(ca.pem.clone()),
    };
    std::fs::write(
        &meta_path,
        serde_json::to_vec(&meta).expect("meta serializes"),
    )?;
    tracing::info!("issued a new TLS certificate");
    Ok((chain.into_bytes(), key.into_bytes()))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CertMeta {
    names: Vec<String>,
    not_after: i64,
    /// The CA certificate (PEM) that issued it.
    #[serde(default)]
    ca: Option<String>,
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

        let ca = load(&source, dir.path())
            .unwrap()
            .certs
            .ca_der()
            .unwrap()
            .to_vec();
        let renamed = TlsSource::SelfSigned {
            hostnames: vec!["host.example".into()],
        };
        let reissued = load(&renamed, dir.path()).unwrap();
        assert_ne!(reissued.certs.info().fingerprint, first.fingerprint);
        assert_eq!(reissued.certs.ca_der().unwrap(), &ca[..]);
    }

    #[test]
    fn server_certificate_is_issued_by_the_host_ca() {
        let dir = tempfile::tempdir().unwrap();
        let source = TlsSource::SelfSigned {
            hostnames: vec!["192.168.1.20".into()],
        };
        let loaded = load(&source, dir.path()).unwrap();
        let chain = std::fs::read(dir.path().join(CERT_FILE)).unwrap();
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&chain)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(certs.len(), 2);
        assert_eq!(certs[1].as_ref(), loaded.certs.ca_der().unwrap());
        let (_, leaf) = x509_parser::parse_x509_certificate(certs[0].as_ref()).unwrap();
        let (_, ca) = x509_parser::parse_x509_certificate(certs[1].as_ref()).unwrap();
        assert_eq!(leaf.issuer(), ca.subject());
        assert!(ca.is_ca());
        assert!(!leaf.is_ca());
        let eku = leaf.extended_key_usage().unwrap().unwrap();
        assert!(eku.value.server_auth);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(certs[1].clone()).unwrap();
        let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            Arc::new(rustls::crypto::ring::default_provider()),
        )
        .build()
        .unwrap();
        use rustls::client::danger::ServerCertVerifier;
        let name = rustls::pki_types::ServerName::try_from("192.168.1.20").unwrap();
        verifier
            .verify_server_cert(
                &certs[0],
                &[],
                &name,
                &[],
                rustls::pki_types::UnixTime::now(),
            )
            .unwrap();
    }
}
