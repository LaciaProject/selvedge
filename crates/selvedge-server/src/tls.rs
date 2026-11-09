//! mTLS between Selvedge's components.
//!
//! With `SELVEDGE_TLS_CERT`, `SELVEDGE_TLS_KEY` and `SELVEDGE_TLS_CA` set,
//! every internal hop is TLS with a certificate on both sides, signed by the
//! platform CA. A certificate names the component's role as a DNS name,
//! `<role>.selvedge.internal`, and each side checks the other's role:
//!
//! | listener            | accepts clients  |
//! | ------------------- | ---------------- |
//! | agent (runner)      | gateway          |
//! | data                | runner           |
//! | API internal        | runner, gateway  |
//! | gateway internal    | API, data        |
//!
//! Runners run tenants' code, so a runner's certificate in particular reaches
//! no other runner and cannot route requests into account processes. A
//! listener also accepts its own role, for health probes run in the pod, but
//! such a client reaches only `/healthz`. Clients check the server's role
//! whatever name or address they dialed: runners are reached by pod IP.
//! The internal token still applies.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, RootCertStore, SignatureScheme};

/// A component's role, as its certificate names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Api,
    Data,
    Gateway,
    Runner,
}

impl Role {
    pub const ALL: [Role; 4] = [Role::Api, Role::Data, Role::Gateway, Role::Runner];

    /// The DNS name a certificate for this role carries.
    pub fn name(self) -> &'static str {
        match self {
            Role::Api => "api.selvedge.internal",
            Role::Data => "data.selvedge.internal",
            Role::Gateway => "gateway.selvedge.internal",
            Role::Runner => "runner.selvedge.internal",
        }
    }

    fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// A set of roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Roles(u8);

impl Roles {
    pub fn of(roles: &[Role]) -> Self {
        Roles(roles.iter().fold(0, |m, r| m | r.bit()))
    }
    pub fn contains(self, role: Role) -> bool {
        self.0 & role.bit() != 0
    }
    pub fn intersects(self, other: Roles) -> bool {
        self.0 & other.0 != 0
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    fn union(self, other: Roles) -> Roles {
        Roles(self.0 | other.0)
    }
    fn iter(self) -> impl Iterator<Item = Role> {
        Role::ALL.into_iter().filter(move |r| self.contains(*r))
    }
}

impl std::fmt::Display for Roles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.iter().map(Role::name).collect();
        f.write_str(&names.join(", "))
    }
}

/// The roles a certificate names. The chain is not checked here.
pub fn roles_of(cert: &CertificateDer<'_>) -> Roles {
    let Ok(ee) = webpki::EndEntityCert::try_from(cert) else { return Roles::default() };
    Roles::of(
        &Role::ALL
            .into_iter()
            .filter(|r| {
                let name = ServerName::try_from(r.name()).expect("valid name");
                ee.verify_is_valid_for_subject_name(&name).is_ok()
            })
            .collect::<Vec<_>>(),
    )
}

pub const AGENT_CLIENTS: &[Role] = &[Role::Gateway];
pub const DATA_CLIENTS: &[Role] = &[Role::Runner];
pub const API_INTERNAL_CLIENTS: &[Role] = &[Role::Runner, Role::Gateway];
pub const GATEWAY_INTERNAL_CLIENTS: &[Role] = &[Role::Api, Role::Data];

pub struct Tls {
    /// The roles this process's own certificate names.
    pub own: Roles,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    roots: Arc<RootCertStore>,
    servers: Mutex<HashMap<Roles, Arc<rustls::ServerConfig>>>,
    clients: Mutex<HashMap<Roles, Arc<rustls::ClientConfig>>>,
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
    if let Some(t) = &tls {
        tracing::info!(roles = %t.own, "mTLS between components is on");
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
    let own = chain.first().map(roles_of).unwrap_or_default();
    if own.is_empty() {
        anyhow::bail!(
            "{cert} names no Selvedge role: its subjectAltName must include one of {}",
            Roles::of(&Role::ALL)
        );
    }
    let tls =
        Tls { own, chain, key, roots: Arc::new(roots), servers: Mutex::default(), clients: Mutex::default() };
    // Fail at startup, not on the first connection.
    tls.server(&[])?;
    tls.client(&Role::ALL)?;
    Ok(tls)
}

impl Tls {
    /// A server config for a listener that accepts `clients` (and this
    /// process's own role, for probes).
    pub fn server(&self, clients: &[Role]) -> Result<Arc<rustls::ServerConfig>> {
        let accepted = Roles::of(clients).union(self.own);
        if let Some(c) = self.servers.lock().expect("lock").get(&accepted) {
            return Ok(c.clone());
        }
        let inner = WebPkiClientVerifier::builder_with_provider(self.roots.clone(), provider()).build()?;
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()?
                .with_client_cert_verifier(Arc::new(ClientRoles { inner, accepted }))
                .with_single_cert(self.chain.clone(), self.key.clone_key())?,
        );
        self.servers.lock().expect("lock").insert(accepted, config.clone());
        Ok(config)
    }

    /// A client config that presents this process's certificate and accepts
    /// servers holding one of `servers`.
    pub fn client(&self, servers: &[Role]) -> Result<Arc<rustls::ClientConfig>> {
        let accepted = Roles::of(servers);
        if let Some(c) = self.clients.lock().expect("lock").get(&accepted) {
            return Ok(c.clone());
        }
        let inner =
            rustls::client::WebPkiServerVerifier::builder_with_provider(self.roots.clone(), provider())
                .build()?;
        let config = Arc::new(
            rustls::ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(ServerRoles { inner, accepted }))
                .with_client_auth_cert(self.chain.clone(), self.key.clone_key())?,
        );
        self.clients.lock().expect("lock").insert(accepted, config.clone());
        Ok(config)
    }
}

/// Verifies a server's chain against the platform CA for one of the accepted
/// role names, whatever name or address was dialed.
#[derive(Debug)]
struct ServerRoles {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    accepted: Roles,
}

impl ServerCertVerifier for ServerRoles {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let mut last = rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName);
        for role in self.accepted.iter() {
            let name = ServerName::try_from(role.name()).expect("valid name");
            match self.inner.verify_server_cert(end_entity, intermediates, &name, ocsp_response, now) {
                Ok(v) => return Ok(v),
                Err(e) => last = e,
            }
        }
        Err(last)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Requires a client certificate from the platform CA that names one of the
/// accepted roles.
#[derive(Debug)]
struct ClientRoles {
    inner: Arc<dyn ClientCertVerifier>,
    accepted: Roles,
}

impl ClientCertVerifier for ClientRoles {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let verified = self.inner.verify_client_cert(end_entity, intermediates, now)?;
        if !roles_of(end_entity).intersects(self.accepted) {
            return Err(rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName));
        }
        Ok(verified)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// A reqwest client builder that, with mTLS, presents this component's
/// certificate and accepts servers holding one of `servers`.
pub fn reqwest(builder: reqwest::ClientBuilder, servers: &[Role]) -> reqwest::ClientBuilder {
    match get() {
        Some(t) => builder.use_preconfigured_tls((*t.client(servers).expect("client config")).clone()),
        None => builder,
    }
}

/// Wraps a dialed TCP stream in TLS when mTLS is on; the server must hold
/// `server`'s certificate.
pub async fn connect(stream: tokio::net::TcpStream, server: Role) -> std::io::Result<Stream> {
    match get() {
        Some(t) => {
            let config = t.client(&[server]).map_err(std::io::Error::other)?;
            let name = ServerName::try_from(server.name()).expect("valid name");
            let s = tokio_rustls::TlsConnector::from(config).connect(name, stream).await?;
            Ok(Stream { inner: Inner::Client(Box::new(s)), health_only: false })
        }
        None => Ok(Stream { inner: Inner::Plain(stream), health_only: false }),
    }
}

/// Accepts TLS on an accepted TCP stream when mTLS is on, from `clients` or
/// this process's own role. A client of the own role only (a probe) gets a
/// stream marked [`Stream::health_only`].
pub async fn accept(stream: tokio::net::TcpStream, clients: &[Role]) -> std::io::Result<Stream> {
    match get() {
        Some(t) => {
            let config = t.server(clients).map_err(std::io::Error::other)?;
            let s = tokio_rustls::TlsAcceptor::from(config).accept(stream).await?;
            let peer =
                s.get_ref().1.peer_certificates().and_then(|c| c.first()).map(roles_of).unwrap_or_default();
            let health_only = !peer.intersects(Roles::of(clients));
            Ok(Stream { inner: Inner::Server(Box::new(s)), health_only })
        }
        None => Ok(Stream { inner: Inner::Plain(stream), health_only: false }),
    }
}

/// Whether a request on a [`Stream::health_only`] connection may proceed.
pub fn health_path(path: &str) -> bool {
    matches!(path, "/healthz" | "/__selvedge/healthz")
}

/// A TCP stream, maybe in TLS.
pub struct Stream {
    inner: Inner,
    health_only: bool,
}

enum Inner {
    Plain(tokio::net::TcpStream),
    Client(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
    Server(Box<tokio_rustls::server::TlsStream<tokio::net::TcpStream>>),
}

impl Stream {
    /// The client holds only the listener's own role: it may reach
    /// `/healthz` and nothing else.
    pub fn health_only(&self) -> bool {
        self.health_only
    }
}

macro_rules! delegate {
    ($self:ident, $s:ident => $e:expr) => {
        match &mut $self.get_mut().inner {
            Inner::Plain($s) => $e,
            Inner::Client($s) => $e,
            Inner::Server($s) => $e,
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

/// An axum listener that, with mTLS, requires a platform certificate of one
/// of `clients` (or of the own role, for probes). Handshakes run off the
/// accept loop, so a slow client holds up no one else. Serve it with
/// `into_make_service_with_connect_info::<Peer>()` and [`health_gate`].
pub struct Listener {
    rx: tokio::sync::mpsc::Receiver<(Stream, std::net::SocketAddr)>,
    local: std::net::SocketAddr,
}

impl Listener {
    pub fn new(listener: tokio::net::TcpListener, clients: &'static [Role]) -> std::io::Result<Self> {
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
                    match tokio::time::timeout(std::time::Duration::from_secs(10), accept(stream, clients))
                        .await
                    {
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

/// What an axum handler knows about a [`Listener`] connection.
#[derive(Clone, Copy, Debug)]
pub struct Peer {
    pub health_only: bool,
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, Listener>> for Peer {
    fn connect_info(stream: axum::serve::IncomingStream<'_, Listener>) -> Self {
        Peer { health_only: stream.io().health_only() }
    }
}

/// Middleware: a [`Peer::health_only`] client reaches `/healthz` only.
pub async fn health_gate(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<Peer>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if peer.health_only && !health_path(req.uri().path()) {
        return axum::response::IntoResponse::into_response((axum::http::StatusCode::FORBIDDEN, "forbidden"));
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_sets() {
        let s = Roles::of(&[Role::Runner, Role::Gateway]);
        assert!(s.contains(Role::Runner) && s.contains(Role::Gateway) && !s.contains(Role::Api));
        assert!(s.intersects(Roles::of(&[Role::Gateway])));
        assert!(!s.intersects(Roles::of(&[Role::Data])));
        assert_eq!(s.to_string(), "gateway.selvedge.internal, runner.selvedge.internal");
    }

    #[test]
    fn no_listener_accepts_runners_except_data_and_api() {
        for (clients, runner_ok) in [
            (AGENT_CLIENTS, false),
            (GATEWAY_INTERNAL_CLIENTS, false),
            (DATA_CLIENTS, true),
            (API_INTERNAL_CLIENTS, true),
        ] {
            assert_eq!(Roles::of(clients).contains(Role::Runner), runner_ok);
        }
    }
}
