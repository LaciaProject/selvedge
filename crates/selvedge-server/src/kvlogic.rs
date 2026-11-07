//! KV rules shared by the REST API and the binding service, matching
//! Cloudflare's documented limits.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub const MAX_KEY_BYTES: usize = 512;
pub const MAX_VALUE_BYTES: usize = 25 << 20;
pub const MAX_METADATA_BYTES: usize = 1024;
pub const MIN_TTL_SECS: i64 = 60;
pub const MAX_LIST_LIMIT: i64 = 1000;

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

pub fn validate_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Err("Key names must not be empty".into());
    }
    if key == "." || key == ".." {
        return Err(format!("Illegal key name \"{key}\". Please use a different name."));
    }
    if key.len() > MAX_KEY_BYTES {
        return Err(format!(
            "UTF-8 encoded length of {} exceeds key length limit of {MAX_KEY_BYTES}.",
            key.len()
        ));
    }
    Ok(())
}

pub fn validate_metadata(metadata: Option<&str>) -> Result<(), String> {
    match metadata {
        Some(m) if m.len() > MAX_METADATA_BYTES => {
            Err(format!("Metadata length of {} exceeds limit of {MAX_METADATA_BYTES}.", m.len()))
        }
        Some(m) if serde_json::from_str::<serde_json::Value>(m).is_err() => {
            Err("Metadata must be valid JSON".into())
        }
        _ => Ok(()),
    }
}

/// Resolves `expiration` (absolute seconds) or `expiration_ttl` (relative
/// seconds) to an absolute expiration, enforcing the 60-second minimum.
pub fn expiration(expiration: Option<&str>, ttl: Option<&str>) -> Result<Option<i64>, String> {
    let now = now_secs();
    if let Some(ttl) = ttl.filter(|s| !s.is_empty()) {
        let ttl: i64 = ttl.parse().map_err(|_| format!("Invalid expiration_ttl of {ttl}."))?;
        if ttl < MIN_TTL_SECS {
            return Err(format!(
                "Invalid expiration_ttl of {ttl}. Expiration TTL must be at least {MIN_TTL_SECS}."
            ));
        }
        return Ok(Some(now + ttl));
    }
    if let Some(exp) = expiration.filter(|s| !s.is_empty()) {
        let exp: i64 = exp.parse().map_err(|_| format!("Invalid expiration of {exp}."))?;
        if exp < now + MIN_TTL_SECS {
            return Err(format!(
                "Invalid expiration of {exp}. Expiration times must be at least {MIN_TTL_SECS} seconds in the future."
            ));
        }
        return Ok(Some(exp));
    }
    Ok(None)
}

pub fn encode_cursor(last_key: &str) -> String {
    URL_SAFE_NO_PAD.encode(last_key)
}

pub fn decode_cursor(cursor: &str) -> Result<String, String> {
    URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| "Invalid cursor".to_owned())
}

pub fn list_limit(raw: Option<&str>) -> Result<i64, String> {
    match raw.filter(|s| !s.is_empty()) {
        None => Ok(MAX_LIST_LIMIT),
        Some(s) => {
            let n: i64 = s.parse().map_err(|_| format!("Invalid key_count_limit of {s}"))?;
            if n < 1 {
                return Err(format!(
                    "Invalid key_count_limit of {n}. Please specify an integer greater than 0."
                ));
            }
            Ok(n.min(MAX_LIST_LIMIT))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        assert!(validate_key("a").is_ok());
        assert!(validate_key("").is_err());
        assert!(validate_key("..").is_err());
        assert!(validate_key(&"x".repeat(513)).is_err());
    }

    #[test]
    fn expirations() {
        assert_eq!(expiration(None, None).unwrap(), None);
        assert!(expiration(None, Some("30")).is_err());
        assert!(expiration(None, Some("60")).unwrap().unwrap() >= now_secs() + 60);
        assert!(expiration(Some("1"), None).is_err());
        let far = (now_secs() + 3600).to_string();
        assert_eq!(expiration(Some(&far), None).unwrap(), Some(far.parse().unwrap()));
    }

    #[test]
    fn cursors() {
        assert_eq!(decode_cursor(&encode_cursor("ké/y")).unwrap(), "ké/y");
        assert!(decode_cursor("!!").is_err());
    }
}
