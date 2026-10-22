//! A certificate browsers trust, without a click-through: Let's Encrypt
//! issues one for the host's DuckDNS name. The host proves it holds the
//! name with a DNS record (ACME DNS-01) that DuckDNS sets on request, so
//! nothing beyond the host's own ports needs forwarding, and the
//! certificate renews on its own once two thirds of its life have passed.
//! The new certificate is served from the next connection on.
//!
//! The same DuckDNS token also keeps the name pointing at this PC's public
//! address (a DDNS client), unless that is turned off.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use instant_acme::{
    Account, AccountBuilder, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use tokio::sync::mpsc;

use crate::Error;
use crate::config::DuckDns;
use crate::server::Notice;
use crate::tls::{ACME_CERT_FILE, ACME_KEY_FILE, CertInfo, CertStore};

const ACCOUNT_FILE: &str = "acme-account.json";
const DUCKDNS: &str = "https://www.duckdns.org";
/// How often the name's address is refreshed.
const ADDRESS_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often the certificate's age is checked.
const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
/// After a failed issue: Let's Encrypt allows 5 failures an hour.
const RETRY_FIRST: Duration = Duration::from_secs(3600);
/// How long DuckDNS gets to publish the TXT record before Let's Encrypt
/// looks it up.
const DNS_WAIT: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long Let's Encrypt gets to check the record and sign.
const ORDER_TIMEOUT: Duration = Duration::from_secs(180);

/// Sets the TXT record at `_acme-challenge.<domain>` that DNS-01 checks.
pub(crate) trait TxtRecord: Sync {
    fn set(&self, value: &str) -> impl Future<Output = Result<(), Error>> + Send;
    fn clear(&self) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Where certificates come from: the directory URL, and a CA to trust for
/// it other than the system's (only a test server needs one).
pub(crate) struct Issuer {
    pub directory: String,
    pub root: Option<PathBuf>,
    pub dns_wait: Duration,
}

impl Issuer {
    fn builder(&self) -> Result<AccountBuilder, Error> {
        match &self.root {
            Some(root) => Account::builder_with_root(root),
            None => Account::builder(),
        }
        .map_err(acme_error)
    }
}

/// Keeps `certs` a current Let's Encrypt certificate for `duck`'s name, and
/// the name's address current. Runs until dropped; problems are reported
/// as [`Notice::Warning`] and retried.
pub(crate) async fn maintain(
    certs: Arc<CertStore>,
    duck: DuckDns,
    data_dir: PathBuf,
    notices: mpsc::UnboundedSender<Notice>,
) {
    let warn = |message: String| {
        tracing::warn!("{message}");
        let _ = notices.send(Notice::Warning(message));
    };
    let api = match DuckDnsApi::new(DUCKDNS, &duck) {
        Ok(api) => api,
        Err(e) => return warn(e.to_string()),
    };
    let issuer = Issuer {
        directory: duck.directory.clone(),
        root: None,
        dns_wait: DNS_WAIT,
    };
    let address = async {
        if !duck.update_address {
            return std::future::pending().await;
        }
        let mut failing = false;
        loop {
            match api.update_address().await {
                Ok(()) if failing => {
                    failing = false;
                    tracing::info!("DuckDNS address updates work again");
                }
                Ok(()) => tracing::debug!("DuckDNS address updated"),
                // Said once, not every few minutes.
                Err(e) if !failing => {
                    failing = true;
                    warn(format!("could not update {}'s address: {e}", api.domain));
                }
                Err(_) => {}
            }
            tokio::time::sleep(ADDRESS_EVERY).await;
        }
    };
    let certificate = async {
        let mut retry = RETRY_FIRST;
        loop {
            let info = certs.info();
            if !renewal_due(&info, now()) {
                tokio::time::sleep(CHECK_EVERY).await;
                continue;
            }
            tracing::info!(domain = %api.domain, "asking Let's Encrypt for a certificate");
            match renew(&certs, &issuer, &data_dir, &api, duck.email.as_deref()).await {
                Ok(info) => {
                    retry = RETRY_FIRST;
                    tracing::info!(domain = %api.domain, fingerprint = %info.fingerprint, "certificate issued");
                    let _ = notices.send(Notice::Certificate {
                        domain: api.domain.clone(),
                        fingerprint: info.fingerprint,
                    });
                }
                Err(e) => {
                    warn(format!(
                        "no certificate for {} yet ({e}); trying again in {} min",
                        api.domain,
                        retry.as_secs() / 60
                    ));
                    tokio::time::sleep(retry).await;
                    retry = (retry * 2).min(CHECK_EVERY);
                }
            }
        }
    };
    tokio::join!(address, certificate);
}

/// Issues a certificate, stores it and serves it.
async fn renew(
    certs: &CertStore,
    issuer: &Issuer,
    data_dir: &Path,
    api: &DuckDnsApi,
    email: Option<&str>,
) -> Result<CertInfo, Error> {
    let (cert, key) = issue(issuer, data_dir, &api.domain, email, api).await?;
    // Checked before anything is stored or served.
    let info = certs.replace(cert.as_bytes(), key.as_bytes())?;
    crate::store::write_private(&data_dir.join(ACME_KEY_FILE), key.as_bytes())?;
    std::fs::write(data_dir.join(ACME_CERT_FILE), cert.as_bytes())?;
    Ok(info)
}

/// A self-signed certificate is replaced at once; a real one once two
/// thirds of its life have passed (30 of Let's Encrypt's 90 days left).
fn renewal_due(info: &CertInfo, now: i64) -> bool {
    let life = info.not_after - info.not_before;
    info.self_signed || now >= info.not_before + life / 3 * 2
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// One order for `domain`: the certificate chain and its key, as PEM.
pub(crate) async fn issue(
    issuer: &Issuer,
    data_dir: &Path,
    domain: &str,
    email: Option<&str>,
    txt: &impl TxtRecord,
) -> Result<(String, String), Error> {
    let account = account(issuer, data_dir, email).await?;
    let identifiers = [Identifier::Dns(domain.to_string())];
    let mut order = account
        .new_order(&NewOrder::new(&identifiers))
        .await
        .map_err(acme_error)?;
    let mut record_set = false;
    let mut authorizations = order.authorizations();
    while let Some(authz) = authorizations.next().await {
        let mut authz = authz.map_err(acme_error)?;
        match authz.status {
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Valid => continue,
            status => return Err(Error::Acme(format!("authorization is {status:?}"))),
        }
        let mut challenge = authz
            .challenge(ChallengeType::Dns01)
            .ok_or_else(|| Error::Acme("no DNS challenge offered".into()))?;
        txt.set(&challenge.key_authorization().dns_value()).await?;
        record_set = true;
        tokio::time::sleep(issuer.dns_wait).await;
        challenge.set_ready().await.map_err(acme_error)?;
    }
    let retries = RetryPolicy::new()
        .initial_delay(Duration::from_secs(2))
        .timeout(ORDER_TIMEOUT);
    let status = order.poll_ready(&retries).await;
    if record_set {
        if let Err(e) = txt.clear().await {
            tracing::debug!("could not clear the challenge record: {e}");
        }
    }
    match status.map_err(acme_error)? {
        OrderStatus::Ready => {}
        status => {
            return Err(Error::Acme(format!(
                "Let's Encrypt could not check the DNS record (order {status:?})"
            )));
        }
    }
    let key = order.finalize().await.map_err(acme_error)?;
    let cert = order.poll_certificate(&retries).await.map_err(acme_error)?;
    Ok((cert, key))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredAccount {
    directory: String,
    credentials: AccountCredentials,
}

/// The ACME account kept in `data_dir`, or a new one (agreeing to the CA's
/// terms) when there is none for this directory yet.
async fn account(issuer: &Issuer, data_dir: &Path, email: Option<&str>) -> Result<Account, Error> {
    let path = data_dir.join(ACCOUNT_FILE);
    let stored: Option<StoredAccount> = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(|s: &StoredAccount| s.directory == issuer.directory);
    if let Some(stored) = stored {
        return issuer
            .builder()?
            .from_credentials(stored.credentials)
            .await
            .map_err(acme_error);
    }
    let contact = email.map(|e| format!("mailto:{e}"));
    let contact: Vec<&str> = contact.iter().map(String::as_str).collect();
    let (account, credentials) = issuer
        .builder()?
        .create(
            &NewAccount {
                contact: &contact,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            issuer.directory.clone(),
            None,
        )
        .await
        .map_err(acme_error)?;
    let stored = StoredAccount {
        directory: issuer.directory.clone(),
        credentials,
    };
    crate::store::write_private(
        &path,
        &serde_json::to_vec(&stored).expect("account serializes"),
    )?;
    Ok(account)
}

fn acme_error(e: instant_acme::Error) -> Error {
    Error::Acme(format!("Let's Encrypt: {e}"))
}

type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

fn http_client() -> Result<HttpClient, Error> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .try_with_platform_verifier()
        .map_err(|e| Error::Acme(format!("TLS roots: {e}")))?
        .https_or_http()
        .enable_http1()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

/// Sends `req` and returns the body as text, if the status is a success.
async fn send(client: &HttpClient, req: hyper::Request<Full<Bytes>>) -> Result<String, Error> {
    let what = format!("{}{}", req.uri().host().unwrap_or(""), req.uri().path());
    let res = tokio::time::timeout(HTTP_TIMEOUT, client.request(req))
        .await
        .map_err(|_| Error::Acme(format!("{what}: no answer")))?
        .map_err(|e| Error::Acme(format!("{what}: {e}")))?;
    let status = res.status();
    let body = tokio::time::timeout(HTTP_TIMEOUT, res.into_body().collect())
        .await
        .map_err(|_| Error::Acme(format!("{what}: no answer")))?
        .map_err(|e| Error::Acme(format!("{what}: {e}")))?
        .to_bytes();
    if !status.is_success() {
        return Err(Error::Acme(format!("{what}: HTTP {status}")));
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// DuckDNS's update API: the name's address, and its TXT record.
struct DuckDnsApi {
    base: String,
    name: String,
    token: String,
    domain: String,
    client: HttpClient,
}

impl DuckDnsApi {
    fn new(base: &str, duck: &DuckDns) -> Result<Self, Error> {
        if !valid_name(&duck.name) {
            return Err(Error::Acme(format!(
                "{:?} is not a DuckDNS name (letters, digits and dashes, without .duckdns.org)",
                duck.name
            )));
        }
        if duck.token.is_empty()
            || !duck
                .token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(Error::Acme(
                "the DuckDNS token is missing or malformed".into(),
            ));
        }
        Ok(Self {
            base: base.to_string(),
            name: duck.name.clone(),
            token: duck.token.clone(),
            domain: duck.domain(),
            client: http_client()?,
        })
    }

    /// One update call. The token is in the URL, so the URL is never logged.
    async fn call(&self, params: &str) -> Result<(), Error> {
        let url = format!(
            "{}/update?domains={}&token={}{params}",
            self.base, self.name, self.token
        );
        let req = hyper::Request::get(url)
            .body(Full::default())
            .map_err(|e| Error::Acme(e.to_string()))?;
        let body = send(&self.client, req).await?;
        if body.trim_start().starts_with("OK") {
            Ok(())
        } else {
            Err(Error::Acme(
                "DuckDNS refused the update; check the name and token".into(),
            ))
        }
    }

    /// Points the name at the address DuckDNS sees the request come from.
    async fn update_address(&self) -> Result<(), Error> {
        self.call("&ip=").await
    }
}

impl TxtRecord for DuckDnsApi {
    // DuckDNS serves one TXT record for the name and everything under it,
    // `_acme-challenge` included. The value is base64url: no escaping.
    async fn set(&self, value: &str) -> Result<(), Error> {
        self.call(&format!("&txt={value}")).await
    }

    async fn clear(&self) -> Result<(), Error> {
        self.call("&txt=&clear=true").await
    }
}

/// A DuckDNS subdomain: what goes before `.duckdns.org`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(not_before: i64, not_after: i64, self_signed: bool) -> CertInfo {
        CertInfo {
            fingerprint: String::new(),
            not_before,
            not_after,
            self_signed,
        }
    }

    #[test]
    fn renews_with_a_third_of_its_life_left() {
        let day = 86_400;
        let lets_encrypt = info(0, 90 * day, false);
        assert!(!renewal_due(&lets_encrypt, 59 * day));
        assert!(renewal_due(&lets_encrypt, 60 * day));
        // A self-signed stand-in goes as soon as a real one can be had.
        assert!(renewal_due(&info(0, 825 * day, true), 1));
    }

    #[test]
    fn duckdns_names_are_checked_before_they_go_in_a_url() {
        assert!(valid_name("my-pc"));
        assert!(!valid_name("my-pc.duckdns.org"));
        assert!(!valid_name("a&token=x"));
        assert!(!valid_name(""));
        assert!(!valid_name("-x"));
        let duck = |name: &str, token: &str| DuckDns {
            name: name.into(),
            token: token.into(),
            email: None,
            directory: String::new(),
            update_address: true,
        };
        assert!(
            DuckDnsApi::new(
                DUCKDNS,
                &duck("my-pc", "a7c4d0ad-1234-5678-9abc-def012345678")
            )
            .is_ok()
        );
        assert!(DuckDnsApi::new(DUCKDNS, &duck("my-pc", "x&ip=1.2.3.4")).is_err());
        assert!(DuckDnsApi::new(DUCKDNS, &duck("My PC", "abc")).is_err());
    }

    /// pebble-challtestsrv's management API standing in for DuckDNS.
    struct ChallTestSrv {
        base: String,
        host: String,
        client: HttpClient,
    }

    impl ChallTestSrv {
        async fn post(&self, path: &str, body: serde_json::Value) -> Result<(), Error> {
            let req = hyper::Request::post(format!("{}{path}", self.base))
                .body(Full::new(Bytes::from(body.to_string())))
                .unwrap();
            send(&self.client, req).await.map(drop)
        }
    }

    impl TxtRecord for ChallTestSrv {
        async fn set(&self, value: &str) -> Result<(), Error> {
            self.post(
                "/set-txt",
                serde_json::json!({"host": self.host, "value": value}),
            )
            .await
        }

        async fn clear(&self) -> Result<(), Error> {
            self.post("/clear-txt", serde_json::json!({"host": self.host}))
                .await
        }
    }

    /// The whole DNS-01 order against Pebble, Let's Encrypt's test server.
    /// Needs `pebble -dnsserver 127.0.0.1:8053` and `pebble-challtestsrv`
    /// running; `WS_PEBBLE` is Pebble's directory URL, `WS_PEBBLE_ROOT` the
    /// CA its HTTPS uses (test/certs/pebble.minica.pem in its repository),
    /// `WS_CHALLTESTSRV` the challenge server's management URL.
    #[tokio::test]
    async fn issues_through_a_dns_challenge() {
        let (Ok(directory), Ok(root), Ok(chall)) = (
            std::env::var("WS_PEBBLE"),
            std::env::var("WS_PEBBLE_ROOT"),
            std::env::var("WS_CHALLTESTSRV"),
        ) else {
            eprintln!("skipped: WS_PEBBLE, WS_PEBBLE_ROOT and WS_CHALLTESTSRV not set");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let issuer = Issuer {
            directory,
            root: Some(root.into()),
            dns_wait: Duration::ZERO,
        };
        let domain = "my-pc.duckdns.org";
        let txt = ChallTestSrv {
            base: chall,
            host: format!("_acme-challenge.{domain}."),
            client: http_client().unwrap(),
        };
        let (cert, key) = issue(&issuer, dir.path(), domain, None, &txt)
            .await
            .unwrap();
        let tls = crate::tls::load(
            &crate::TlsSource::SelfSigned {
                hostnames: Vec::new(),
            },
            dir.path(),
        )
        .unwrap();
        let info = tls.certs.replace(cert.as_bytes(), key.as_bytes()).unwrap();
        assert!(!info.self_signed);
        assert!(!renewal_due(&info, now()));
        // The account is kept: a second order reuses it.
        assert!(dir.path().join(ACCOUNT_FILE).exists());
        issue(&issuer, dir.path(), domain, None, &txt)
            .await
            .unwrap();
    }
}
