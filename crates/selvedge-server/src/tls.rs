//! mTLS between Selvedge's components.
//!
//! With `SELVEDGE_TLS_CERT`, `SELVEDGE_TLS_KEY` and `SELVEDGE_TLS_CA` set,
//! every internal hop (gateway to agent, agent to data and API, API and data
//! to the gateway's internal listener) is TLS with a certificate on both
//! sides, signed by the platform CA. Certificates name one host,
//! `selvedge.internal`, which every client expects whatever address it
//! dialed: runners are reached by pod IP, and a certificate from the platform
//! CA is what identifies a component. The internal token still applies.

use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};

/// The name every component certificate carries and every client checks.
pub const SERVER_NAME: &str = "selvedge.internal";

pub struct Tls {
    /// For internal-only listeners: a client certificate is required.
    pub server: Arc<rustls::ServerConfig>,
    pub client: Arc<rustls::ClientConfig>,
}

static TLS: OnceLock<Option<Tls>> = OnceLock::new();

/// This process's TLS identity, if mTLS is configured.
pub fn get() -> Option<&'static Tls> {
    TLS.get_or_init(|| None).as_ref()
}

/// Loads the identity from `SELVEDGE_TLS_*` (once, at startup).
pub fn init_from_env() -> Result<()> {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let tls = match (var("SELVEDGE_TLS_CERT"), var("SELVEDGE_TLS_KEY"), var("SELVEDGE_TLS_CA")) {
        (Some(c), Some(k), Some(ca)) => Some(load(&c, &k, &ca)?),
        (None, None, None) => None,
        _ => anyhow::bail!("set all of SELVEDGE_TLS_CERT, SELVEDGE_TLS_KEY and SELVEDGE_TLS_CA, or none"),
    };
    if tls.is_some() {
        tracing::info!("mTLS between components is on");
    }
    let _ = TLS.set(tls);
    Ok(())
}

/// `https` with mTLS, else `http`.
pub fn scheme() -> &'static str {
    if get().is_some() { "https" } else { "http" }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn load(cert: &str, key: &str, ca: &str) -> Result<Tls> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {cert}"))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing {cert}"))?;
    let key = PrivateKeyDer::from_pem_file(key).with_context(|| format!("reading {key}"))?;
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(ca).with_context(|| format!("reading {ca}"))? {
        roots.add(c.with_context(|| format!("parsing {ca}"))?)?;
    }
    let roots = Arc::new(roots);

    let required = WebPkiClientVerifier::builder_with_provider(roots.clone(), provider()).build()?;
    let server = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(required)
        .with_single_cert(chain.clone(), key.clone_key())?;

    let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(roots, provider()).build()?;
    let client = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(FixedName(inner)))
        .with_client_auth_cert(chain, key)?;
    Ok(Tls { server: Arc::new(server), client: Arc::new(client) })
}

/// Verifies the chain against the platform CA for `selvedge.internal`,
/// whatever name or address was dialed.
#[derive(Debug)]
struct FixedName(Arc<rustls::client::WebPkiServerVerifier>);

impl ServerCertVerifier for FixedName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let name = ServerName::try_from(SERVER_NAME).expect("valid name");
        self.0.verify_server_cert(end_entity, intermediates, &name, ocsp_response, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// A reqwest client builder that, with mTLS, presents this component's
/// certificate and trusts only the platform CA.
pub fn reqwest(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    match get() {
        Some(t) => builder.use_preconfigured_tls((*t.client).clone()),
        None => builder,
    }
}

/// Wraps a dialed TCP stream in TLS when mTLS is on.
pub async fn connect(stream: tokio::net::TcpStream) -> std::io::Result<Stream> {
    match get() {
        Some(t) => {
            let name = ServerName::try_from(SERVER_NAME).expect("valid name");
            let s = tokio_rustls::TlsConnector::from(t.client.clone()).connect(name, stream).await?;
            Ok(Stream::Client(Box::new(s)))
        }
        None => Ok(Stream::Plain(stream)),
    }
}

/// Accepts TLS on an accepted TCP stream when mTLS is on.
pub async fn accept(stream: tokio::net::TcpStream) -> std::io::Result<Stream> {
    match get() {
        Some(t) => {
            let s = tokio_rustls::TlsAcceptor::from(t.server.clone()).accept(stream).await?;
            Ok(Stream::Server(Box::new(s)))
        }
        None => Ok(Stream::Plain(stream)),
    }
}

/// A TCP stream, maybe in TLS.
pub enum Stream {
    Plain(tokio::net::TcpStream),
    Client(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
    Server(Box<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>),
}

macro_rules! delegate {
    ($self:ident, $s:ident => $e:expr) => {
        match $self.get_mut() {
            Stream::Plain($s) => $e,
            Stream::Client($s) => $e,
            Stream::Server($s) => $e,
        }
    };
}

impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate!(self, s => std::pin::Pin::new(s).poll_read(cx, buf))
    }
}

impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        delegate!(self, s => std::pin::Pin::new(s).poll_write(cx, buf))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate!(self, s => std::pin::Pin::new(s).poll_flush(cx))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        delegate!(self, s => std::pin::Pin::new(s).poll_shutdown(cx))
    }
}

/// An axum listener that, with mTLS, requires a platform certificate from
/// every client. Handshakes run off the accept loop, so a slow client holds
/// up no one else.
pub struct Listener {
    rx: tokio::sync::mpsc::Receiver<(Stream, std::net::SocketAddr)>,
    local: std::net::SocketAddr,
}

impl Listener {
    pub fn new(listener: tokio::net::TcpListener) -> std::io::Result<Self> {
        let local = listener.local_addr()?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    match tokio::time::timeout(std::time::Duration::from_secs(10), accept(stream)).await {
                        Ok(Ok(s)) => {
                            let _ = tx.send((s, peer)).await;
                        }
                        Ok(Err(e)) => tracing::debug!(%peer, error = %e, "TLS handshake failed"),
                        Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
                    }
                });
            }
        });
        Ok(Self { rx, local })
    }
}

impl axum::serve::Listener for Listener {
    type Io = Stream;
    type Addr = std::net::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            if let Some(a) = self.rx.recv().await {
                return a;
            }
            // The accept task ended; nothing more will come.
            std::future::pending::<()>().await;
        }
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}
