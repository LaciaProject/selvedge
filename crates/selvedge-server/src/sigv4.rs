//! AWS Signature Version 4, as S3 clients sign requests: for the agent's
//! Durable Object storage proxy and R2's S3-compatible API.

use hmac::{Hmac, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use sha2::{Digest, Sha256};

/// Everything but RFC 3986 unreserved characters, as SigV4 encodes.
pub const AWS: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');
pub const AWS_PATH: &AsciiSet = &AWS.remove(b'/');

type HmacSha256 = Hmac<Sha256>;

pub fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("HMAC takes any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

pub fn canonical_query(query: &str) -> String {
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            let dec = |s: &str| percent_decode_str(&s.replace('+', "%20")).decode_utf8_lossy().into_owned();
            (utf8_percent_encode(&dec(k), AWS).to_string(), utf8_percent_encode(&dec(v), AWS).to_string())
        })
        .collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// The SigV4 signature of a request.
#[allow(clippy::too_many_arguments)]
pub fn signature(
    secret: &str,
    method: &str,
    canonical_path: &str,
    query: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    amz_date: &str,
    region: &str,
) -> String {
    let signed: Vec<&str> = headers.iter().map(|(k, _)| k.as_str()).collect();
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let canonical = format!(
        "{method}\n{canonical_path}\n{}\n{canonical_headers}\n{}\n{payload_hash}",
        canonical_query(query),
        signed.join(";")
    );
    let date = &amz_date[..8.min(amz_date.len())];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign =
        format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", hex::encode(Sha256::digest(canonical)));
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k = hmac(&k, region.as_bytes());
    let k = hmac(&k, b"s3");
    let k = hmac(&k, b"aws4_request");
    hex::encode(hmac(&k, to_sign.as_bytes()))
}

pub struct Authorization {
    pub access_key_id: String,
    pub region: String,
    pub signed_headers: Vec<String>,
    pub signature: String,
}

pub fn parse_authorization(v: &str) -> Option<Authorization> {
    let rest = v.strip_prefix("AWS4-HMAC-SHA256 ")?;
    let (mut cred, mut signed, mut sig) = (None, None, None);
    for part in rest.split(',') {
        let (k, v) = part.trim().split_once('=')?;
        match k {
            "Credential" => cred = Some(v),
            "SignedHeaders" => signed = Some(v),
            "Signature" => sig = Some(v),
            _ => {}
        }
    }
    let mut c = cred?.split('/');
    let access_key_id = c.next()?.to_owned();
    let _date = c.next()?;
    let region = c.next()?.to_owned();
    Some(Authorization {
        access_key_id,
        region,
        signed_headers: signed?.split(';').map(str::to_owned).collect(),
        signature: sig?.to_owned(),
    })
}

pub fn amz_date_secs(d: &str) -> Option<i64> {
    let dt = chrono::NaiveDateTime::parse_from_str(d, "%Y%m%dT%H%M%SZ").ok()?;
    Some(dt.and_utc().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_signature_test_vector() {
        // AWS's documented example: GET /test.txt with a range header.
        let headers = vec![
            ("host".to_owned(), "examplebucket.s3.amazonaws.com".to_owned()),
            ("range".to_owned(), "bytes=0-9".to_owned()),
            (
                "x-amz-content-sha256".to_owned(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
            ),
            ("x-amz-date".to_owned(), "20130524T000000Z".to_owned()),
        ];
        let sig = signature(
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "GET",
            "/test.txt",
            "",
            &headers,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "20130524T000000Z",
            "us-east-1",
        );
        assert_eq!(sig, "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41");
    }

    #[test]
    fn query_canonicalization() {
        assert_eq!(canonical_query("prefix=a%2Fb%2F&list-type=2"), "list-type=2&prefix=a%2Fb%2F");
        assert_eq!(canonical_query("uploads"), "uploads=");
    }
}
