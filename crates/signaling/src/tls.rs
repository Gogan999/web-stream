use std::path::Path;
use std::sync::{Arc, RwLock};

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
/// Where a certificate from Let's Encrypt is kept, with its key and the
/// directory that issued it, in one file written at once (see
/// [`crate::acme`]).
pub(crate) const ACME_FILE: &str = "acme-cert.json";
const VALIDITY: time::Duration = time::Duration::days(825);
const RENEW_BEFORE: time::Duration = time::Duration::days(30);

pub struct LoadedTls {
    pub config: Arc<ServerConfig>,
    /// The certificate in use, which [`crate::acme`] can replace while the
    /// server runs.
    pub certs: Arc<CertStore>,
}

pub fn load(source: &TlsSource, data_dir: &Path) -> Result<LoadedTls, Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let store = |(cert, key): (Vec<u8>, Vec<u8>)| CertStore::new(provider.clone(), &cert, &key);
    let certs = match source {
        TlsSource::Files { cert, key } => store((std::fs::read(cert)?, std::fs::read(key)?))?,
        TlsSource::SelfSigned { hostnames } => store(self_signed(data_dir, hostnames)?)?,
        TlsSource::DuckDns(duck) => {
            let domain = duck.domain();
            // Until Let's Encrypt has issued one (or if what is stored is
            // unusable): browsers warn as with any self-signed certificate,
            // but the host is reachable, and a new one is ordered at once.
            let stored = stored_acme(data_dir, &domain, &duck.directory).and_then(|pair| {
                store(pair)
                    .inspect_err(|e| tracing::warn!("stored Let's Encrypt certificate: {e}"))
                    .ok()
            });
            match stored {
                Some(certs) => certs,
                None => {
                    let mut names = duck.hostnames.clone();
                    names.push(domain);
                    store(self_signed(data_dir, &names)?)?
                }
            }
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

/// The certificate the server presents, swapped in place on renewal so the
/// listener never restarts.
#[derive(Debug)]
pub struct CertStore {
    provider: Arc<CryptoProvider>,
    current: RwLock<Current>,
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
    fn new(provider: Arc<CryptoProvider>, cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, Error> {
        let current = parse(&provider, cert_pem, key_pem)?;
        Ok(Self {
            provider,
            current: RwLock::new(current),
        })
    }

    /// Serves `cert_pem` from the next handshake on.
    pub fn replace(&self, cert_pem: &[u8], key_pem: &[u8]) -> Result<CertInfo, Error> {
        let next = parse(&self.provider, cert_pem, key_pem)?;
        let info = next.info.clone();
        *self.current.write().unwrap_or_else(|e| e.into_inner()) = next;
        Ok(info)
    }

    pub fn info(&self) -> CertInfo {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .info
            .clone()
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(
            self.current
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .key
                .clone(),
        )
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

/// A certificate from Let's Encrypt, as stored.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct StoredCert {
    /// The ACME directory that issued it: a staging certificate isn't kept
    /// once staging is turned off.
    pub directory: String,
    pub cert: String,
    pub key: String,
}

/// Stores a certificate and its key in one go, so a crash can never leave
/// one without the other.
pub(crate) fn store_acme(data_dir: &Path, stored: &StoredCert) -> Result<(), Error> {
    let json = serde_json::to_vec(stored).expect("certificate serializes");
    crate::store::write_private(&data_dir.join(ACME_FILE), &json)
}

/// The stored Let's Encrypt certificate for `domain` from `directory`,
/// unless it has expired (then a self-signed one serves while a new one is
/// issued).
fn stored_acme(data_dir: &Path, domain: &str, directory: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let stored: StoredCert =
        serde_json::from_slice(&std::fs::read(data_dir.join(ACME_FILE)).ok()?).ok()?;
    if stored.directory != directory {
        return None;
    }
    let leaf = CertificateDer::pem_slice_iter(stored.cert.as_bytes())
        .next()?
        .ok()?;
    let (_, cert) = x509_parser::parse_x509_certificate(leaf.as_ref()).ok()?;
    let names_domain = cert
        .subject_alternative_name()
        .ok()
        .flatten()
        .is_some_and(|san| {
            san.value.general_names.iter().any(|n| {
                matches!(n, x509_parser::extensions::GeneralName::DNSName(d)
                    if d.eq_ignore_ascii_case(domain))
            })
        });
    let unexpired =
        cert.validity().not_after.timestamp() > time::OffsetDateTime::now_utc().unix_timestamp();
    (names_domain && unexpired).then(|| (stored.cert.into_bytes(), stored.key.into_bytes()))
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

    #[test]
    fn a_stored_certificate_serves_only_for_its_name_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let domain = "my-pc.duckdns.org";
        let duck = |directory: &str| {
            TlsSource::DuckDns(crate::DuckDns {
                name: "my-pc".into(),
                token: "t".into(),
                email: None,
                directory: directory.into(),
                update_address: false,
                hostnames: vec!["192.168.1.20".into()],
            })
        };
        // Nothing stored: a self-signed stand-in.
        let stand_in = load(&duck("prod"), dir.path()).unwrap().certs.info();
        assert!(stand_in.self_signed);
        // (Self-signed, so it stands in for an issued one here.)
        let (cert, key) = self_signed(other.path(), &[domain.into()]).unwrap();
        let stored = |directory: &str, key: &[u8]| StoredCert {
            directory: directory.into(),
            cert: String::from_utf8(cert.clone()).unwrap(),
            key: String::from_utf8(key.to_vec()).unwrap(),
        };
        store_acme(dir.path(), &stored("prod", &key)).unwrap();
        let issued = load(&duck("prod"), dir.path()).unwrap().certs.info();
        assert_ne!(issued.fingerprint, stand_in.fingerprint);
        // From staging, once staging is off: the stand-in again.
        let staging = load(&duck("staging"), dir.path()).unwrap().certs.info();
        assert_eq!(staging.fingerprint, stand_in.fingerprint);
        // A key that isn't the certificate's: the stand-in, not a failure.
        let (_, wrong) = self_signed(tempfile::tempdir().unwrap().path(), &[]).unwrap();
        store_acme(dir.path(), &stored("prod", &wrong)).unwrap();
        let broken = load(&duck("prod"), dir.path()).unwrap().certs.info();
        assert_eq!(broken.fingerprint, stand_in.fingerprint);
    }

    #[test]
    fn a_replaced_certificate_serves_from_then_on() {
        let dir = tempfile::tempdir().unwrap();
        let tls = load(
            &TlsSource::SelfSigned {
                hostnames: Vec::new(),
            },
            dir.path(),
        )
        .unwrap();
        let before = tls.certs.info();
        let other = tempfile::tempdir().unwrap();
        let (cert, key) = self_signed(other.path(), &["x.example".into()]).unwrap();
        let after = tls.certs.replace(&cert, &key).unwrap();
        assert_ne!(after.fingerprint, before.fingerprint);
        assert_eq!(tls.certs.info(), after);
        // A key that isn't the certificate's is refused, and the old pair stays.
        let (_, wrong_key) = self_signed(dir.path(), &["y.example".into()]).unwrap();
        assert!(tls.certs.replace(&cert, &wrong_key).is_err());
        assert_eq!(tls.certs.info(), after);
    }
}
