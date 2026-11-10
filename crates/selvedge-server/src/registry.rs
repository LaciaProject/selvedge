//! Access to the container image registry (RFC 0002 §4.4).
//!
//! The operator runs an OCI registry with Docker's token authentication, and
//! the Selvedge API is its token service. Clients never hold a registry
//! account: `wrangler` asks the API for a credential, a short-lived password
//! that names an account and what it may do, signed with a key derived from
//! the internal token. `docker login` then presents it to the token service
//! (`GET /registry/token`), which answers with a signed token granting those
//! actions on `<account id>/*` only. Nodes pull with a pull-only credential
//! the containers controller makes for each instance.

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use selvedge_core::AccountId;
use serde_json::json;
use sha2::Sha256;

/// The user name credentials are issued with, as Cloudflare's are. The token
/// service ignores it: the password says everything.
pub const USERNAME: &str = "v1";
const PREFIX: &str = "slvr1";

/// What a credential allows on its account's repositories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Permissions {
    pub pull: bool,
    pub push: bool,
}

impl Permissions {
    pub const PULL: Self = Self { pull: true, push: false };

    fn encode(self) -> &'static str {
        match (self.pull, self.push) {
            (true, true) => "rw",
            (true, false) => "r",
            (false, true) => "w",
            (false, false) => "-",
        }
    }

    fn decode(s: &str) -> Option<Self> {
        Some(match s {
            "rw" => Self { pull: true, push: true },
            "r" => Self { pull: true, push: false },
            "w" => Self { pull: false, push: true },
            "-" => Self { pull: false, push: false },
            _ => return None,
        })
    }

    fn allows(self, action: &str) -> bool {
        match action {
            "pull" => self.pull,
            "push" => self.push,
            _ => false,
        }
    }
}

/// Signs and checks registry credentials. Every component with the internal
/// token derives the same key.
#[derive(Clone)]
pub struct Credentials {
    key: [u8; 32],
}

impl Credentials {
    pub fn new(internal_token: &str) -> Self {
        let mut mac = Hmac::<Sha256>::new_from_slice(internal_token.as_bytes()).expect("any key length");
        mac.update(b"selvedge registry credentials v1");
        Self { key: mac.finalize().into_bytes().into() }
    }

    fn sign(&self, body: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("any key length");
        mac.update(body.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// A password for `account`, valid until `expires` (Unix seconds).
    pub fn issue(&self, account: &AccountId, permissions: Permissions, expires: u64) -> String {
        let body = format!("{PREFIX}.{account}.{}.{expires}", permissions.encode());
        format!("{body}.{}", self.sign(&body))
    }

    /// The account and permissions of a password, if it is genuine and has
    /// not expired.
    pub fn verify(&self, password: &str, now: u64) -> Option<(AccountId, Permissions)> {
        let (body, signature) = password.rsplit_once('.')?;
        if !crate::constant_time_eq(self.sign(body).as_bytes(), signature.as_bytes()) {
            return None;
        }
        let mut parts = body.split('.');
        if parts.next()? != PREFIX {
            return None;
        }
        let account = parts.next()?.parse().ok()?;
        let permissions = Permissions::decode(parts.next()?)?;
        let expires: u64 = parts.next()?.parse().ok()?;
        (parts.next().is_none() && now < expires).then_some((account, permissions))
    }
}

/// The token service's signing identity: an ECDSA P-256 key and its
/// certificate, which the registry trusts (`auth.token.rootcertbundle`).
pub struct TokenSigner {
    key: EcdsaKeyPair,
    /// The certificate, as tokens carry it (`x5c`).
    certificate: String,
    /// `auth.token.issuer` and `auth.token.service` in the registry's
    /// configuration.
    pub issuer: String,
    pub service: String,
}

/// How long a registry token is valid. Pushing a large layer takes one token
/// per request, not per upload, so this need not cover a whole push.
const TOKEN_TTL: u64 = 300;

impl TokenSigner {
    /// Loads the key (PEM: PKCS#8 `PRIVATE KEY` or SEC1 `EC PRIVATE KEY`)
    /// and its certificate (PEM).
    pub fn load(key_pem: &str, cert_pem: &str, issuer: String, service: String) -> Result<Self> {
        let pkcs8 = match PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .context("reading the registry token key")?
        {
            PrivateKeyDer::Pkcs8(k) => k.secret_pkcs8_der().to_vec(),
            PrivateKeyDer::Sec1(k) => sec1_to_pkcs8(k.secret_sec1_der()),
            _ => bail!("the registry token key must be an ECDSA P-256 key"),
        };
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_FIXED_SIGNING,
            &pkcs8,
            &ring::rand::SystemRandom::new(),
        )
        .map_err(|e| anyhow::anyhow!("the registry token key is not a usable ECDSA P-256 key: {e}"))?;
        let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .context("reading the registry token certificate")?;
        Ok(Self { key, certificate: STANDARD.encode(cert.as_ref()), issuer, service })
    }

    /// A token for the scopes of a request (`repository:<name>:<actions>`)
    /// that the credential covers: repositories under the account's own
    /// prefix, and the actions it permits. Scopes it does not cover are left
    /// out, which is how the registry learns the request is denied.
    pub fn token(
        &self,
        account: &AccountId,
        permissions: Permissions,
        scopes: &[String],
        now: u64,
    ) -> String {
        let prefix = format!("{account}/");
        let access: Vec<_> = scopes
            .iter()
            .flat_map(|s| s.split(' '))
            .filter_map(|scope| {
                let mut parts = scope.splitn(3, ':');
                let (ty, name, actions) = (parts.next()?, parts.next()?, parts.next()?);
                if ty != "repository" || !name.starts_with(&prefix) {
                    return None;
                }
                let granted: Vec<&str> = actions.split(',').filter(|a| permissions.allows(a)).collect();
                (!granted.is_empty()).then(|| json!({ "type": ty, "name": name, "actions": granted }))
            })
            .collect();
        let header = json!({ "typ": "JWT", "alg": "ES256", "x5c": [self.certificate] });
        let claims = json!({
            "iss": self.issuer,
            "sub": account.as_str(),
            "aud": self.service,
            "iat": now,
            "nbf": now.saturating_sub(30),
            "exp": now + TOKEN_TTL,
            "jti": hex::encode(rand::random::<[u8; 16]>()),
            "access": access,
        });
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = self
            .key
            .sign(&ring::rand::SystemRandom::new(), signing_input.as_bytes())
            .expect("signing with a loaded key");
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
    }

    pub const fn ttl() -> u64 {
        TOKEN_TTL
    }
}

fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match content.len() {
        n if n < 0x80 => out.push(n as u8),
        n if n < 0x100 => out.extend([0x81, n as u8]),
        n => out.extend([0x82, (n >> 8) as u8, n as u8]),
    }
    out.extend(content);
    out
}

/// Wraps a SEC1 `ECPrivateKey` (what `openssl ecparam -genkey` and Helm's
/// `genPrivateKey "ecdsa"` write) as PKCS#8 for the P-256 curve.
fn sec1_to_pkcs8(sec1: &[u8]) -> Vec<u8> {
    // AlgorithmIdentifier { id-ecPublicKey, prime256v1 }
    const ALGORITHM: &[u8] = &[
        0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03,
        0x01, 0x07,
    ];
    let mut body = vec![0x02, 0x01, 0x00];
    body.extend(der(0x30, ALGORITHM));
    body.extend(der(0x04, sec1));
    der(0x30, &body)
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountId {
        "0123456789abcdef0123456789abcdef".parse().unwrap()
    }

    #[test]
    fn credentials_round_trip() {
        let c = Credentials::new("token");
        let p = c.issue(&account(), Permissions { pull: true, push: true }, 1000);
        assert_eq!(c.verify(&p, 999), Some((account(), Permissions { pull: true, push: true })));
        // Expired, forged, or signed for another installation.
        assert_eq!(c.verify(&p, 1000), None);
        assert_eq!(c.verify(&p.replace(".rw.", ".r."), 1), None);
        assert_eq!(Credentials::new("other").verify(&p, 1), None);
        assert_eq!(c.verify("garbage", 1), None);
    }

    // A throwaway key: openssl ecparam -name prime256v1 -genkey -noout
    const KEY: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIOz1GwWvgtz1weclIvaAzylbZ4Y+3AAXQddaBGcHHejRoAoGCCqGSM49
AwEHoUQDQgAEnUT1/x2WtZB3XGazcA3A4p2qOmb57Lrw3VTNj+GJr6Zh8bsjappP
4jzzuI3Wd9sfz/bx/GqIyvKyAZ/akAAUjA==
-----END EC PRIVATE KEY-----
";

    #[test]
    fn sec1_keys_load() {
        let pkcs8 = match PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap() {
            PrivateKeyDer::Sec1(k) => sec1_to_pkcs8(k.secret_sec1_der()),
            _ => panic!("expected a SEC1 key"),
        };
        EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &pkcs8, &ring::rand::SystemRandom::new())
            .unwrap();
    }

    #[test]
    fn tokens_cover_only_the_account() {
        let pkcs8 = match PrivateKeyDer::from_pem_slice(KEY.as_bytes()).unwrap() {
            PrivateKeyDer::Sec1(k) => sec1_to_pkcs8(k.secret_sec1_der()),
            _ => unreachable!(),
        };
        let signer = TokenSigner {
            key: EcdsaKeyPair::from_pkcs8(
                &ECDSA_P256_SHA256_FIXED_SIGNING,
                &pkcs8,
                &ring::rand::SystemRandom::new(),
            )
            .unwrap(),
            certificate: "Y2VydA==".into(),
            issuer: "selvedge".into(),
            service: "registry".into(),
        };
        let scopes = vec![
            format!("repository:{}/app:pull,push", account()),
            "repository:ffffffffffffffffffffffffffffffff/app:pull".to_owned(),
            "registry:catalog:*".to_owned(),
        ];
        let token = signer.token(&account(), Permissions::PULL, &scopes, 100);
        let claims: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token.split('.').nth(1).unwrap()).unwrap())
                .unwrap();
        assert_eq!(
            claims["access"],
            json!([{ "type": "repository", "name": format!("{}/app", account()), "actions": ["pull"] }])
        );
        assert_eq!(claims["exp"], 400);
        assert_eq!(claims["aud"], "registry");
    }
}
