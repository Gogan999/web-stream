use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Settings for the embedded HTTPS server.
#[derive(Debug, Clone)]
pub struct SignalingConfig {
    /// Address the HTTPS listener binds to.
    pub bind: SocketAddr,
    /// Where the TLS certificate (when self-signed) and paired devices are kept.
    pub data_dir: PathBuf,
    /// Directory with the web client. `None` serves `embedded_web`, or a
    /// placeholder page when that is empty.
    pub web_root: Option<PathBuf>,
    /// The web client built into the binary, as (path, contents).
    pub embedded_web: &'static [(&'static str, &'static [u8])],
    pub tls: TlsSource,
    /// External TURN servers handed to the browser. Usually empty, since the
    /// host is reachable directly.
    pub turn: Vec<TurnServer>,
    /// A paired device unused for this long is revoked.
    pub device_idle_expiry: Duration,
    /// Extra origins allowed to call the API besides the host's own, for
    /// example a DDNS name behind a reverse proxy.
    pub extra_origins: Vec<String>,
}

impl SignalingConfig {
    pub fn new(bind: SocketAddr, data_dir: PathBuf) -> Self {
        Self {
            bind,
            data_dir,
            web_root: None,
            embedded_web: &[],
            tls: TlsSource::SelfSigned {
                hostnames: Vec::new(),
            },
            turn: Vec::new(),
            device_idle_expiry: Duration::from_secs(30 * 24 * 3600),
            extra_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TlsSource {
    /// Generate a certificate on first run and keep it in `data_dir`.
    /// `hostnames` are extra names or IPs to put in it besides localhost.
    SelfSigned { hostnames: Vec<String> },
    /// PEM files, for example from Let's Encrypt.
    Files { cert: PathBuf, key: PathBuf },
    /// A Let's Encrypt certificate for a DuckDNS name, issued and renewed
    /// by the host itself (see [`DuckDns`]).
    DuckDns(DuckDns),
}

pub const LETS_ENCRYPT: &str = "https://acme-v02.api.letsencrypt.org/directory";
pub const LETS_ENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// `name.duckdns.org`, kept pointing at this PC's public address, with a
/// certificate from Let's Encrypt. The certificate is proven with a DNS
/// record (ACME DNS-01) that DuckDNS sets on request, so no port beyond the
/// host's own needs forwarding, and it renews on its own.
#[derive(Clone)]
pub struct DuckDns {
    /// The DuckDNS subdomain, without `.duckdns.org`.
    pub name: String,
    /// The DuckDNS account token.
    pub token: String,
    /// Where Let's Encrypt sends expiry warnings (optional).
    pub email: Option<String>,
    /// The ACME directory: [`LETS_ENCRYPT`], or [`LETS_ENCRYPT_STAGING`]
    /// to try things out without its rate limits.
    pub directory: String,
    /// Also update the name's address every few minutes (a DDNS client).
    pub update_address: bool,
}

impl DuckDns {
    pub fn domain(&self) -> String {
        format!("{}.duckdns.org", self.name)
    }
}

impl std::fmt::Debug for DuckDns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuckDns")
            .field("name", &self.name)
            .field("token", &"<redacted>")
            .field("email", &self.email)
            .field("directory", &self.directory)
            .field("update_address", &self.update_address)
            .finish()
    }
}

/// A TURN server using the shared-secret REST credential scheme (coturn
/// `use-auth-secret`).
#[derive(Clone)]
pub struct TurnServer {
    pub urls: Vec<String>,
    pub shared_secret: String,
    /// How long issued credentials stay valid.
    pub ttl: Duration,
}

impl std::fmt::Debug for TurnServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnServer")
            .field("urls", &self.urls)
            .field("shared_secret", &"<redacted>")
            .field("ttl", &self.ttl)
            .finish()
    }
}
