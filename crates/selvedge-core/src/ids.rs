//! Identifiers. Cloudflare uses 32 lowercase hex characters for account,
//! namespace and version identifiers; Selvedge does the same so that clients
//! that validate the format keep working.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdError {
    #[error("identifier must be 32 lowercase hex characters")]
    BadHex,
    #[error("script name must be 1-63 characters of a-z, 0-9, '-' or '_', and not start with '-'")]
    BadScriptName,
}

/// A 32-character lowercase hex identifier.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HexId(String);

impl HexId {
    pub fn random() -> Self {
        let bytes: [u8; 16] = rand::random();
        Self(hex::encode(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for HexId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            Ok(Self(s.to_owned()))
        } else {
            Err(IdError::BadHex)
        }
    }
}

impl TryFrom<String> for HexId {
    type Error = IdError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<HexId> for String {
    fn from(id: HexId) -> String {
        id.0
    }
}

impl fmt::Display for HexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for HexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

pub type AccountId = HexId;
pub type NamespaceId = HexId;
/// Version ids are UUIDs, as Cloudflare's are; clients pass them back verbatim.
pub type VersionId = String;

/// A random (version 4) UUID.
pub fn new_version_id() -> VersionId {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Script names follow Cloudflare's rules closely enough that every name
/// Cloudflare accepts and Selvedge can route is accepted here.
pub fn validate_script_name(name: &str) -> Result<(), IdError> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && !name.starts_with('-')
        && name.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_'));
    if ok { Ok(()) } else { Err(IdError::BadScriptName) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_ids() {
        let id = HexId::random();
        assert_eq!(id.as_str().len(), 32);
        assert_eq!(id.as_str().parse::<HexId>().unwrap(), id);
        assert_eq!("ABCDEF00ABCDEF00ABCDEF00ABCDEF00".parse::<HexId>(), Err(IdError::BadHex));
        assert_eq!("abc".parse::<HexId>(), Err(IdError::BadHex));
    }

    #[test]
    fn version_ids() {
        let v = new_version_id();
        assert_eq!(v.len(), 36);
        assert_eq!(&v[14..15], "4");
        assert_ne!(v, new_version_id());
    }

    #[test]
    fn script_names() {
        assert!(validate_script_name("hello-world_2").is_ok());
        assert!(validate_script_name("").is_err());
        assert!(validate_script_name("-x").is_err());
        assert!(validate_script_name("Hello").is_err());
        assert!(validate_script_name(&"a".repeat(64)).is_err());
    }
}
