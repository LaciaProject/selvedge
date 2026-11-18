//! OpenID Connect login (RFC 0003 §4.1): authorization code flow with PKCE,
//! as a confidential (or public) client configured from the provider's
//! discovery document. ID tokens are checked as OpenID Connect Core §3.1.3.7
//! says, signatures with `ring`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::signature;
use serde::Deserialize;
use serde_json::{Map, Value};
use tokio::sync::RwLock;

/// How the provider is configured.
#[derive(Clone, Debug)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    /// None for a public client.
    pub client_secret: Option<String>,
    /// For the login button.
    pub name: String,
    pub scopes: Vec<String>,
    /// Where the groups are: a claim name, or a dotted path into nested
    /// claims (`realm_access.roles`).
    pub groups_claim: String,
    pub admin_groups: Vec<String>,
    /// Verified emails of platform admins.
    pub admin_emails: Vec<String>,
}

/// The provider's metadata (OpenID Connect Discovery 1.0 §3).
#[derive(Debug, Deserialize)]
pub struct Metadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    pub userinfo_endpoint: Option<String>,
    pub end_session_endpoint: Option<String>,
    #[serde(default)]
    pub id_token_signing_alg_values_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Clone)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    #[serde(rename = "use")]
    use_: Option<String>,
    alg: Option<String>,
    // RSA
    n: Option<String>,
    e: Option<String>,
    // EC and OKP
    crv: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

struct Keys {
    keys: Vec<Jwk>,
    fetched: Instant,
}

/// Who logged in, from the ID token and, where it lacks something, the
/// userinfo endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
    pub groups: Vec<String>,
}

impl Identity {
    pub fn is_admin(&self, cfg: &OidcConfig) -> bool {
        self.groups.iter().any(|g| cfg.admin_groups.contains(g))
            || (self.email_verified
                && self
                    .email
                    .as_ref()
                    .is_some_and(|e| cfg.admin_emails.iter().any(|a| a.eq_ignore_ascii_case(e))))
    }
}

/// The signature algorithms Selvedge checks. Never `none`, never HMAC: a
/// client secret must not be able to mint ID tokens.
const SUPPORTED_ALGS: &[&str] =
    &["RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "EdDSA"];

/// Clock skew allowed for `exp` and `iat`.
const LEEWAY_SECS: i64 = 60;

/// The keys are fetched again for an unknown key id, at most this often.
const JWKS_REFRESH: Duration = Duration::from_secs(60);

pub struct Provider {
    pub cfg: OidcConfig,
    http: reqwest::Client,
    meta: RwLock<Option<Arc<Metadata>>>,
    keys: RwLock<Option<Keys>>,
}

impl Provider {
    pub fn new(cfg: OidcConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { cfg, http, meta: RwLock::new(None), keys: RwLock::new(None) })
    }

    /// The metadata, fetched on first use. A provider that is down when the
    /// API starts does not stop it: the next login tries again.
    pub async fn metadata(&self) -> Result<Arc<Metadata>> {
        if let Some(m) = self.meta.read().await.clone() {
            return Ok(m);
        }
        let url = format!("{}/.well-known/openid-configuration", self.cfg.issuer.trim_end_matches('/'));
        let m: Metadata = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("fetching {url}"))?
            .json()
            .await
            .with_context(|| format!("reading {url}"))?;
        // Discovery §4.3: the issuer in the document must be the one asked.
        ensure!(
            m.issuer == self.cfg.issuer,
            "the provider says its issuer is {}, not {}",
            m.issuer,
            self.cfg.issuer
        );
        let m = Arc::new(m);
        *self.meta.write().await = Some(m.clone());
        Ok(m)
    }

    /// Where to send the browser to log in.
    pub async fn authorization_url(
        &self,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        verifier: &str,
    ) -> Result<String> {
        let m = self.metadata().await?;
        let mut url = url::Url::parse(&m.authorization_endpoint).context("authorization_endpoint")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &self.cfg.scopes.join(" "))
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", &pkce_challenge(verifier))
            .append_pair("code_challenge_method", "S256");
        Ok(url.into())
    }

    /// Where to send the browser to log out of the provider too, if it says.
    pub async fn logout_url(&self, post_logout_redirect_uri: &str) -> Option<String> {
        let m = self.metadata().await.ok()?;
        let mut url = url::Url::parse(m.end_session_endpoint.as_deref()?).ok()?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.cfg.client_id)
            .append_pair("post_logout_redirect_uri", post_logout_redirect_uri);
        Some(url.into())
    }

    /// Exchanges the authorization code and checks the ID token. Returns who
    /// logged in.
    pub async fn finish(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        nonce: &str,
    ) -> Result<Identity> {
        let m = self.metadata().await?;
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
        ];
        let mut req = self.http.post(&m.token_endpoint);
        match &self.cfg.client_secret {
            Some(secret) => {
                let methods = m.token_endpoint_auth_methods_supported.as_deref().unwrap_or_default();
                // client_secret_basic is the default (Core §9).
                if methods.is_empty() || methods.iter().any(|x| x == "client_secret_basic") {
                    // RFC 6749 §2.3.1: both parts form-encoded first.
                    req = req.basic_auth(form_encode(&self.cfg.client_id), Some(form_encode(secret)));
                } else if methods.iter().any(|x| x == "client_secret_post") {
                    form.push(("client_id", &self.cfg.client_id));
                    form.push(("client_secret", secret));
                } else {
                    bail!("the provider takes neither client_secret_basic nor client_secret_post");
                }
            }
            None => form.push(("client_id", &self.cfg.client_id)),
        }
        let resp = req.form(&form).send().await.context("calling the token endpoint")?;
        let status = resp.status();
        let body: Value = resp.json().await.context("reading the token response")?;
        ensure!(status.is_success(), "the token endpoint answered {status}: {body}");
        let id_token = body["id_token"].as_str().context("the token response has no id_token")?;
        let claims = self.verify_id_token(&m, id_token, nonce).await?;

        let mut id = identity(&claims, &self.cfg.groups_claim)?;
        // Some providers keep the email or the groups for the userinfo endpoint.
        let lacks_groups = claim_path(&claims, &self.cfg.groups_claim).is_none();
        if let (Some(endpoint), Some(access), true) =
            (&m.userinfo_endpoint, body["access_token"].as_str(), lacks_groups || id.email.is_none())
        {
            match self.userinfo(endpoint, access).await {
                Ok(info) if info.get("sub").and_then(Value::as_str) == Some(id.subject.as_str()) => {
                    let more = identity(&info, &self.cfg.groups_claim)?;
                    if id.email.is_none() {
                        (id.email, id.email_verified) = (more.email, more.email_verified);
                    }
                    if lacks_groups {
                        id.groups = more.groups;
                    }
                    id.name = id.name.or(more.name);
                }
                // Core §5.3.2: a different sub must not be used.
                Ok(_) => bail!("the userinfo endpoint answered for someone else"),
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "OIDC userinfo request failed"),
            }
        }
        Ok(id)
    }

    async fn userinfo(&self, endpoint: &str, access_token: &str) -> Result<Map<String, Value>> {
        Ok(self.http.get(endpoint).bearer_auth(access_token).send().await?.error_for_status()?.json().await?)
    }

    async fn verify_id_token(&self, m: &Metadata, jwt: &str, nonce: &str) -> Result<Map<String, Value>> {
        let allowed: Vec<&str> = if m.id_token_signing_alg_values_supported.is_empty() {
            vec!["RS256"] // Core §15.1: every provider supports it
        } else {
            m.id_token_signing_alg_values_supported.iter().map(String::as_str).collect()
        };
        let parsed = Jwt::parse(jwt)?;
        let key = self.key_for(m, &parsed).await?;
        parsed.verify(&key, &allowed)?;
        check_claims(&parsed.claims, &m.issuer, &self.cfg.client_id, nonce, now())?;
        Ok(parsed.claims)
    }

    /// The key that signed the token, fetching the provider's keys again for
    /// an unknown key id (it rotated them), but not too often.
    async fn key_for(&self, m: &Metadata, jwt: &Jwt) -> Result<Jwk> {
        let stale = |k: &Option<Keys>| k.as_ref().is_none_or(|k| k.fetched.elapsed() >= JWKS_REFRESH);
        for attempt in 0..2 {
            if let Some(k) = self.keys.read().await.as_ref().and_then(|k| pick_key(&k.keys, jwt)) {
                return Ok(k);
            }
            if attempt == 1 || !stale(&*self.keys.read().await) {
                break;
            }
            let set: JwkSet = self
                .http
                .get(&m.jwks_uri)
                .send()
                .await
                .and_then(reqwest::Response::error_for_status)
                .context("fetching the provider's keys")?
                .json()
                .await
                .context("reading the provider's keys")?;
            *self.keys.write().await = Some(Keys { keys: set.keys, fetched: Instant::now() });
        }
        bail!("no key of the provider matches the ID token (kid {:?})", jwt.kid)
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn form_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// A random value for `state`, `nonce` and the PKCE verifier.
pub fn random_token() -> String {
    B64.encode(rand::random::<[u8; 32]>())
}

pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest;
    B64.encode(sha2::Sha256::digest(verifier.as_bytes()))
}

struct Jwt {
    alg: String,
    kid: Option<String>,
    signed: String,
    signature: Vec<u8>,
    claims: Map<String, Value>,
}

impl Jwt {
    fn parse(jwt: &str) -> Result<Self> {
        let mut parts = jwt.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            bail!("the ID token is not a signed JWT");
        };
        let header: Map<String, Value> = serde_json::from_slice(&B64.decode(h).context("ID token header")?)?;
        let claims: Map<String, Value> = serde_json::from_slice(&B64.decode(p).context("ID token payload")?)?;
        Ok(Self {
            alg: header.get("alg").and_then(Value::as_str).unwrap_or_default().to_owned(),
            kid: header.get("kid").and_then(Value::as_str).map(str::to_owned),
            signed: format!("{h}.{p}"),
            signature: B64.decode(s).context("ID token signature")?,
            claims,
        })
    }

    fn verify(&self, key: &Jwk, allowed: &[&str]) -> Result<()> {
        ensure!(
            SUPPORTED_ALGS.contains(&self.alg.as_str()) && allowed.contains(&self.alg.as_str()),
            "ID token signed with {:?}, which is not accepted",
            self.alg
        );
        if let Some(a) = &key.alg {
            ensure!(*a == self.alg, "the key is for {a}, the token says {}", self.alg);
        }
        let b = |v: &Option<String>, what: &str| -> Result<Vec<u8>> {
            B64.decode(v.as_deref().ok_or_else(|| anyhow!("the key has no {what}"))?)
                .with_context(|| what.to_owned())
        };
        let msg = self.signed.as_bytes();
        let bad = |_| anyhow!("the ID token's signature does not verify");
        match (self.alg.as_str(), key.kty.as_str()) {
            (alg @ ("RS256" | "RS384" | "RS512" | "PS256" | "PS384" | "PS512"), "RSA") => {
                let params: &signature::RsaParameters = match alg {
                    "RS256" => &signature::RSA_PKCS1_2048_8192_SHA256,
                    "RS384" => &signature::RSA_PKCS1_2048_8192_SHA384,
                    "RS512" => &signature::RSA_PKCS1_2048_8192_SHA512,
                    "PS256" => &signature::RSA_PSS_2048_8192_SHA256,
                    "PS384" => &signature::RSA_PSS_2048_8192_SHA384,
                    _ => &signature::RSA_PSS_2048_8192_SHA512,
                };
                let (n, e) = (b(&key.n, "n")?, b(&key.e, "e")?);
                signature::RsaPublicKeyComponents { n: &n, e: &e }
                    .verify(params, msg, &self.signature)
                    .map_err(bad)
            }
            (alg @ ("ES256" | "ES384"), "EC") => {
                let (curve, algorithm): (_, &'static dyn signature::VerificationAlgorithm) = match alg {
                    "ES256" => ("P-256", &signature::ECDSA_P256_SHA256_FIXED),
                    _ => ("P-384", &signature::ECDSA_P384_SHA384_FIXED),
                };
                ensure!(key.crv.as_deref() == Some(curve), "an {alg} token needs a {curve} key");
                let mut point = vec![4u8];
                point.extend(b(&key.x, "x")?);
                point.extend(b(&key.y, "y")?);
                signature::UnparsedPublicKey::new(algorithm, point).verify(msg, &self.signature).map_err(bad)
            }
            ("EdDSA", "OKP") => {
                ensure!(key.crv.as_deref() == Some("Ed25519"), "only Ed25519 EdDSA keys are supported");
                signature::UnparsedPublicKey::new(&signature::ED25519, b(&key.x, "x")?)
                    .verify(msg, &self.signature)
                    .map_err(bad)
            }
            (alg, kty) => bail!("an {alg} token cannot be checked with an {kty} key"),
        }
    }
}

/// The signing key for a token: by key id, or the one signing key of the
/// right type if the token names none.
fn pick_key(keys: &[Jwk], jwt: &Jwt) -> Option<Jwk> {
    let kty = match jwt.alg.get(..2) {
        Some("RS" | "PS") => "RSA",
        Some("ES") => "EC",
        _ if jwt.alg == "EdDSA" => "OKP",
        _ => return None,
    };
    let usable = |k: &&Jwk| k.kty == kty && k.use_.as_deref().is_none_or(|u| u == "sig");
    match &jwt.kid {
        Some(kid) => keys.iter().filter(usable).find(|k| k.kid.as_ref() == Some(kid)).cloned(),
        None => {
            let mut it = keys.iter().filter(usable);
            match (it.next(), it.next()) {
                (Some(k), None) => Some(k.clone()),
                _ => None,
            }
        }
    }
}

/// Core §3.1.3.7, steps 2-3 and 9-11.
fn check_claims(c: &Map<String, Value>, issuer: &str, client_id: &str, nonce: &str, now: i64) -> Result<()> {
    ensure!(c.get("iss").and_then(Value::as_str) == Some(issuer), "the ID token is from another issuer");
    let aud: Vec<&str> = match c.get("aud") {
        Some(Value::String(a)) => vec![a.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    ensure!(aud.contains(&client_id), "the ID token is for another client");
    let azp = c.get("azp").and_then(Value::as_str);
    ensure!(aud.len() == 1 || azp.is_some(), "the ID token has several audiences and no azp");
    ensure!(azp.is_none_or(|a| a == client_id), "the ID token was issued to another client (azp)");
    let exp = c.get("exp").and_then(Value::as_i64).context("the ID token has no exp")?;
    ensure!(now < exp + LEEWAY_SECS, "the ID token has expired");
    let iat = c.get("iat").and_then(Value::as_i64).context("the ID token has no iat")?;
    ensure!(iat <= now + LEEWAY_SECS, "the ID token was issued in the future");
    ensure!(c.get("nonce").and_then(Value::as_str) == Some(nonce), "the ID token's nonce does not match");
    ensure!(c.get("sub").and_then(Value::as_str).is_some_and(|s| !s.is_empty()), "the ID token has no sub");
    Ok(())
}

/// A claim by name, or by a dotted path into objects.
fn claim_path<'a>(claims: &'a Map<String, Value>, path: &str) -> Option<&'a Value> {
    if let Some(v) = claims.get(path) {
        return Some(v);
    }
    let mut parts = path.split('.');
    let mut v = claims.get(parts.next()?)?;
    for p in parts {
        v = v.get(p)?;
    }
    Some(v)
}

fn identity(claims: &Map<String, Value>, groups_claim: &str) -> Result<Identity> {
    let s = |k: &str| claims.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_owned);
    Ok(Identity {
        subject: s("sub").context("no sub")?,
        email: s("email"),
        // Some providers send it as a string.
        email_verified: matches!(claims.get("email_verified"), Some(Value::Bool(true)))
            || claims.get("email_verified").and_then(Value::as_str) == Some("true"),
        name: s("name").or_else(|| s("preferred_username")),
        groups: match claim_path(claims, groups_claim) {
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_owned).collect(),
            Some(Value::String(g)) => {
                g.split([' ', ',']).filter(|g| !g.is_empty()).map(str::to_owned).collect()
            }
            _ => vec![],
        },
    })
}

#[cfg(test)]
mod tests {
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, Ed25519KeyPair, KeyPair};
    use serde_json::json;

    use super::*;

    const ISS: &str = "https://id.example.com";
    const CLIENT: &str = "selvedge";

    fn b64(v: &Value) -> String {
        B64.encode(serde_json::to_vec(v).unwrap())
    }

    fn claims() -> Value {
        json!({ "iss": ISS, "aud": CLIENT, "sub": "u1", "nonce": "n1", "iat": 1000, "exp": 2000 })
    }

    struct Es256 {
        key: EcdsaKeyPair,
        jwk: Jwk,
    }

    fn es256(kid: &str) -> Es256 {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        let p = key.public_key().as_ref();
        let jwk = Jwk {
            kty: "EC".into(),
            kid: Some(kid.into()),
            use_: Some("sig".into()),
            alg: None,
            n: None,
            e: None,
            crv: Some("P-256".into()),
            x: Some(B64.encode(&p[1..33])),
            y: Some(B64.encode(&p[33..65])),
        };
        Es256 { key, jwk }
    }

    fn sign_es256(k: &Es256, header: Value, claims: &Value) -> String {
        let signed = format!("{}.{}", b64(&header), b64(claims));
        let sig = k.key.sign(&SystemRandom::new(), signed.as_bytes()).unwrap();
        format!("{signed}.{}", B64.encode(sig.as_ref()))
    }

    fn check(jwt: &str, keys: &[Jwk], allowed: &[&str], now: i64) -> Result<()> {
        let parsed = Jwt::parse(jwt)?;
        let key = pick_key(keys, &parsed).context("no key")?;
        parsed.verify(&key, allowed)?;
        check_claims(&parsed.claims, ISS, CLIENT, "n1", now)
    }

    #[test]
    fn accepts_a_good_token() {
        let k = es256("k1");
        let jwt = sign_es256(&k, json!({ "alg": "ES256", "kid": "k1" }), &claims());
        check(&jwt, std::slice::from_ref(&k.jwk), &["ES256"], 1500).unwrap();
        // Without a kid, the one EC key is used.
        let jwt = sign_es256(&k, json!({ "alg": "ES256" }), &claims());
        check(&jwt, &[k.jwk], &["ES256"], 1500).unwrap();
    }

    #[test]
    fn refuses_a_tampered_or_foreign_signature() {
        let (k, other) = (es256("k1"), es256("k1"));
        let jwt = sign_es256(&other, json!({ "alg": "ES256", "kid": "k1" }), &claims());
        assert!(check(&jwt, std::slice::from_ref(&k.jwk), &["ES256"], 1500).is_err());
        // Claims changed after signing.
        let jwt = sign_es256(&k, json!({ "alg": "ES256", "kid": "k1" }), &claims());
        let mut parts: Vec<String> = jwt.split('.').map(str::to_owned).collect();
        let mut c = claims();
        c["sub"] = json!("admin");
        parts[1] = b64(&c);
        assert!(check(&parts.join("."), &[k.jwk], &["ES256"], 1500).is_err());
    }

    #[test]
    fn refuses_none_hmac_and_unlisted_algorithms() {
        let k = es256("k1");
        let none = format!("{}.{}.", b64(&json!({ "alg": "none", "kid": "k1" })), b64(&claims()));
        assert!(check(&none, std::slice::from_ref(&k.jwk), &["ES256", "none"], 1500).is_err());
        let hs = format!("{}.{}.c2ln", b64(&json!({ "alg": "HS256", "kid": "k1" })), b64(&claims()));
        assert!(check(&hs, std::slice::from_ref(&k.jwk), &["HS256"], 1500).is_err());
        // Supported, but not what the provider says it uses.
        let jwt = sign_es256(&k, json!({ "alg": "ES256", "kid": "k1" }), &claims());
        assert!(check(&jwt, &[k.jwk], &["RS256"], 1500).is_err());
    }

    #[test]
    fn refuses_an_unknown_key_id() {
        let k = es256("k1");
        let jwt = sign_es256(&k, json!({ "alg": "ES256", "kid": "k2" }), &claims());
        assert!(check(&jwt, &[k.jwk], &["ES256"], 1500).is_err());
    }

    #[test]
    fn checks_the_claims() {
        let ok = claims();
        check_claims(ok.as_object().unwrap(), ISS, CLIENT, "n1", 1500).unwrap();
        let bad = |change: &dyn Fn(&mut Value), now: i64| {
            let mut c = claims();
            change(&mut c);
            check_claims(c.as_object().unwrap(), ISS, CLIENT, "n1", now).is_err()
        };
        assert!(bad(&|c| c["iss"] = json!("https://evil.example.com"), 1500));
        assert!(bad(&|c| c["aud"] = json!("other"), 1500));
        assert!(bad(&|c| c["aud"] = json!([CLIENT, "other"]), 1500), "several audiences need azp");
        assert!(!bad(
            &|c| {
                c["aud"] = json!([CLIENT, "other"]);
                c["azp"] = json!(CLIENT)
            },
            1500
        ));
        assert!(bad(&|c| c["azp"] = json!("other"), 1500));
        assert!(bad(&|_| {}, 2000 + LEEWAY_SECS), "expired");
        assert!(!bad(&|_| {}, 2000 + LEEWAY_SECS - 1), "within the leeway");
        assert!(bad(&|c| c["iat"] = json!(1500 + LEEWAY_SECS + 1), 1500), "issued in the future");
        assert!(bad(&|c| c["nonce"] = json!("n2"), 1500));
        assert!(bad(
            &|c| {
                c.as_object_mut().unwrap().remove("nonce");
            },
            1500
        ));
        assert!(bad(&|c| c["sub"] = json!(""), 1500));
    }

    #[test]
    fn verifies_ed25519_and_rsa() {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let jwk = Jwk {
            kty: "OKP".into(),
            kid: None,
            use_: None,
            alg: None,
            n: None,
            e: None,
            crv: Some("Ed25519".into()),
            x: Some(B64.encode(key.public_key().as_ref())),
            y: None,
        };
        let signed = format!("{}.{}", b64(&json!({ "alg": "EdDSA" })), b64(&claims()));
        let jwt = format!("{signed}.{}", B64.encode(key.sign(signed.as_bytes()).as_ref()));
        check(&jwt, &[jwk], &["EdDSA"], 1500).unwrap();

        let rsa = ring::rsa::KeyPair::from_pkcs8(&B64.decode(RSA_TEST_KEY).unwrap()).unwrap();
        let comps: ring::rsa::PublicKeyComponents<Vec<u8>> = rsa.public().into();
        let jwk = Jwk {
            kty: "RSA".into(),
            kid: Some("r1".into()),
            use_: Some("sig".into()),
            alg: Some("RS256".into()),
            n: Some(B64.encode(&comps.n)),
            e: Some(B64.encode(&comps.e)),
            crv: None,
            x: None,
            y: None,
        };
        let signed = format!("{}.{}", b64(&json!({ "alg": "RS256", "kid": "r1" })), b64(&claims()));
        let mut sig = vec![0; rsa.public().modulus_len()];
        rsa.sign(&signature::RSA_PKCS1_SHA256, &rng, signed.as_bytes(), &mut sig).unwrap();
        let jwt = format!("{signed}.{}", B64.encode(&sig));
        check(&jwt, std::slice::from_ref(&jwk), &["RS256"], 1500).unwrap();
        // The key is for RS256 only.
        let signed = format!("{}.{}", b64(&json!({ "alg": "PS256", "kid": "r1" })), b64(&claims()));
        rsa.sign(&signature::RSA_PSS_SHA256, &rng, signed.as_bytes(), &mut sig).unwrap();
        let jwt = format!("{signed}.{}", B64.encode(&sig));
        assert!(check(&jwt, &[jwk], &["PS256"], 1500).is_err());
    }

    #[test]
    fn reads_identity_and_groups() {
        let c = json!({
            "sub": "u1", "email": "a@example.com", "email_verified": "true", "preferred_username": "alice",
            "groups": ["dev", "ops"], "realm_access": { "roles": ["admins"] }, "scope_groups": "a b,c",
        });
        let c = c.as_object().unwrap();
        let id = identity(c, "groups").unwrap();
        assert_eq!(id.email.as_deref(), Some("a@example.com"));
        assert!(id.email_verified);
        assert_eq!(id.name.as_deref(), Some("alice"));
        assert_eq!(id.groups, ["dev", "ops"]);
        assert_eq!(identity(c, "realm_access.roles").unwrap().groups, ["admins"]);
        assert_eq!(identity(c, "scope_groups").unwrap().groups, ["a", "b", "c"]);
        assert!(identity(c, "missing").unwrap().groups.is_empty());

        let cfg = OidcConfig {
            issuer: ISS.into(),
            client_id: CLIENT.into(),
            client_secret: None,
            name: "SSO".into(),
            scopes: vec![],
            groups_claim: "groups".into(),
            admin_groups: vec!["ops".into()],
            admin_emails: vec!["ROOT@example.com".into()],
        };
        assert!(id.is_admin(&cfg));
        let mut other = id.clone();
        other.groups = vec!["dev".into()];
        assert!(!other.is_admin(&cfg));
        other.email = Some("root@example.com".into());
        assert!(other.is_admin(&cfg));
        other.email_verified = false;
        assert!(!other.is_admin(&cfg), "an unverified email does not make an admin");
    }

    #[test]
    fn pkce_matches_rfc_7636() {
        // RFC 7636, appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// A 2048-bit RSA key in PKCS#8, for tests only.
    const RSA_TEST_KEY: &str = include_str!("testdata/rsa-test-key.b64");
}
