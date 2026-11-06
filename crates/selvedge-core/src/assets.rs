//! Workers Static Assets: the upload manifest, the binary manifest Cloudflare's
//! asset worker reads, and the deploy-time parsing of `_headers`, `_redirects`
//! and `run_worker_first` that Cloudflare's API does.
//!
//! The parsers follow `workers-shared/utils/configuration` in workers-sdk
//! (MIT OR Apache-2.0), so a file means the same thing here as on Cloudflare.
//! Rules are kept as ordered lists: the asset worker tries dynamic redirects and
//! header rules in file order, and PostgreSQL's `jsonb` does not keep key order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Largest single asset, as on Cloudflare.
pub const MAX_ASSET_SIZE: u64 = 25 << 20;
/// Most assets in one version, as on Cloudflare.
pub const MAX_ASSET_COUNT: usize = 100_000;

const HEADER_SIZE: usize = 20;
const ENTRY_SIZE: usize = 40;
const MAX_LINE_LENGTH: usize = 2000;
const MAX_HEADER_RULES: usize = 100;
const MAX_DYNAMIC_REDIRECT_RULES: usize = 100;
const MAX_STATIC_REDIRECT_RULES: usize = 2000;
const MAX_ROUTES_RULES: usize = 100;
const MAX_ROUTES_RULE_LENGTH: usize = 100;
const PERMITTED_STATUS_CODES: [u16; 6] = [200, 301, 302, 303, 307, 308];

/// One file of an upload manifest, keyed by its path (`/dir/file.html`).
/// `hash` is wrangler's: 32 hex digits of BLAKE3 over the base64 content and
/// the extension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub hash: String,
    pub size: u64,
}

pub type UploadManifest = BTreeMap<String, ManifestEntry>;

pub fn valid_hash(hash: &str) -> bool {
    hash.len() == 32 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Checks an upload manifest against Cloudflare's limits.
pub fn validate_manifest(manifest: &UploadManifest) -> Result<(), String> {
    if manifest.len() > MAX_ASSET_COUNT {
        return Err(format!("at most {MAX_ASSET_COUNT} assets are allowed in a version"));
    }
    for (path, entry) in manifest {
        if !path.starts_with('/') || path.len() > 4096 || path.contains('\0') {
            return Err(format!("invalid asset path `{path}`"));
        }
        if !valid_hash(&entry.hash) {
            return Err(format!("invalid hash for asset `{path}`"));
        }
        if entry.size > MAX_ASSET_SIZE {
            return Err(format!("asset `{path}` is larger than 25 MiB"));
        }
    }
    Ok(())
}

/// Encodes the binary manifest the asset worker binary-searches: a 20-byte
/// header, then per asset 16 bytes of SHA-256(path), 16 bytes of content hash
/// and 8 bytes of padding, sorted by path hash.
pub fn encode_manifest(manifest: &UploadManifest) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut entries: Vec<([u8; 16], [u8; 16])> = manifest
        .iter()
        .map(|(path, e)| {
            let mut p = [0u8; 16];
            p.copy_from_slice(&Sha256::digest(path.as_bytes())[..16]);
            let mut c = [0u8; 16];
            // Validated as 32 hex digits on upload.
            hex::decode_to_slice(&e.hash, &mut c).unwrap_or_default();
            (p, c)
        })
        .collect();
    entries.sort();
    let mut out = vec![0u8; HEADER_SIZE + entries.len() * ENTRY_SIZE];
    for (i, (p, c)) in entries.iter().enumerate() {
        let at = HEADER_SIZE + i * ENTRY_SIZE;
        out[at..at + 16].copy_from_slice(p);
        out[at + 16..at + 32].copy_from_slice(c);
    }
    out
}

/// The `assets.config` object of upload metadata.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct UploadConfig {
    pub html_handling: Option<String>,
    pub not_found_handling: Option<String>,
    #[serde(default)]
    pub run_worker_first: Option<Value>,
    #[serde(rename = "_redirects")]
    pub redirects: Option<String>,
    #[serde(rename = "_headers")]
    pub headers: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Redirect {
    pub from: String,
    pub status: u16,
    pub to: String,
    pub line_number: usize,
    /// Static rules (no splat or placeholder, before any dynamic rule) are
    /// looked up exactly; dynamic ones are tried in order.
    pub is_static: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeaderRule {
    pub path: String,
    pub set: Vec<(String, String)>,
    pub unset: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaticRouting {
    pub user_worker: Vec<String>,
    pub asset_worker: Vec<String>,
}

/// A version's assets, as stored and sent to agents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScriptAssets {
    /// SHA-256 of the encoded manifest blob.
    pub manifest: String,
    pub count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_handling: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_found_handling: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirects: Vec<Redirect>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<HeaderRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_worker_first: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub static_routing: Option<StaticRouting>,
}

/// Builds a JSON object in the given key order.
fn ordered_object(entries: impl IntoIterator<Item = (String, String)>) -> String {
    let body: Vec<String> = entries.into_iter().map(|(k, v)| format!("{}:{v}", Value::String(k))).collect();
    format!("{{{}}}", body.join(","))
}

impl ScriptAssets {
    /// Parses the upload's `assets.config` into a stored form.
    pub fn from_upload(manifest: String, count: usize, cfg: &UploadConfig) -> Result<Self, String> {
        let html_handling = cfg.html_handling.clone();
        if let Some(h) = &html_handling
            && !["auto-trailing-slash", "force-trailing-slash", "drop-trailing-slash", "none"]
                .contains(&h.as_str())
        {
            return Err(format!("invalid assets.html_handling `{h}`"));
        }
        if let Some(n) = &cfg.not_found_handling
            && !["single-page-application", "404-page", "none"].contains(&n.as_str())
        {
            return Err(format!("invalid assets.not_found_handling `{n}`"));
        }
        let (run_worker_first, static_routing) = match &cfg.run_worker_first {
            None | Some(Value::Null) => (None, None),
            Some(Value::Bool(b)) => (Some(*b), None),
            Some(Value::Array(rules)) => {
                let rules: Vec<String> = rules
                    .iter()
                    .map(|r| r.as_str().map(str::to_owned).ok_or("run_worker_first rules must be strings"))
                    .collect::<Result<_, _>>()?;
                (None, Some(parse_static_routing(&rules)?))
            }
            Some(_) => return Err("assets.run_worker_first must be a boolean or a list of rules".into()),
        };
        Ok(Self {
            manifest,
            count,
            redirects: cfg
                .redirects
                .as_deref()
                .map(|t| parse_redirects(t, html_handling.as_deref()))
                .unwrap_or_default(),
            headers: cfg.headers.as_deref().map(parse_headers).unwrap_or_default(),
            html_handling,
            not_found_handling: cfg.not_found_handling.clone(),
            run_worker_first,
            static_routing,
        })
    }

    /// The asset worker's `CONFIG`.
    pub fn asset_worker_config(&self, compatibility_date: &str, flags: &[String]) -> String {
        let mut fields = vec![
            ("compatibility_date".to_owned(), json!(compatibility_date).to_string()),
            ("compatibility_flags".to_owned(), json!(flags).to_string()),
            ("has_static_routing".to_owned(), json!(self.static_routing.is_some()).to_string()),
        ];
        if let Some(h) = &self.html_handling {
            fields.push(("html_handling".to_owned(), json!(h).to_string()));
        }
        if let Some(n) = &self.not_found_handling {
            fields.push(("not_found_handling".to_owned(), json!(n).to_string()));
        }
        if !self.redirects.is_empty() {
            let rule = |r: &Redirect, with_line: bool| {
                let mut o = json!({ "status": r.status, "to": r.to });
                if with_line {
                    o["lineNumber"] = json!(r.line_number);
                }
                (r.from.clone(), o.to_string())
            };
            let statics =
                ordered_object(self.redirects.iter().filter(|r| r.is_static).map(|r| rule(r, true)));
            let dynamic =
                ordered_object(self.redirects.iter().filter(|r| !r.is_static).map(|r| rule(r, false)));
            fields.push((
                "redirects".to_owned(),
                format!("{{\"version\":1,\"staticRules\":{statics},\"rules\":{dynamic}}}"),
            ));
        }
        if !self.headers.is_empty() {
            let rules = ordered_object(self.headers.iter().map(|h| {
                let mut o = serde_json::Map::new();
                if !h.set.is_empty() {
                    o.insert(
                        "set".into(),
                        Value::Object(h.set.iter().map(|(k, v)| (k.clone(), json!(v))).collect()),
                    );
                }
                if !h.unset.is_empty() {
                    o.insert("unset".into(), json!(h.unset));
                }
                (h.path.clone(), Value::Object(o).to_string())
            }));
            fields.push(("headers".to_owned(), format!("{{\"version\":2,\"rules\":{rules}}}")));
        }
        ordered_object(fields)
    }

    /// The router worker's `CONFIG`.
    pub fn router_config(&self, has_user_worker: bool) -> String {
        let mut o = json!({ "has_user_worker": has_user_worker });
        if let Some(b) = self.run_worker_first {
            o["invoke_user_worker_ahead_of_assets"] = json!(b);
        }
        if let Some(s) = &self.static_routing {
            o["static_routing"] = json!({ "user_worker": s.user_worker, "asset_worker": s.asset_worker });
        }
        o.to_string()
    }
}

// ---- URL validation (validateURL.ts) ----

/// `new URL("//" + path, "relative://")`, then its pathname (and optionally
/// search and hash).
fn extract_pathname(path: &str, include_search: bool, include_hash: bool) -> Option<String> {
    let path = if path.starts_with('/') { path.to_owned() } else { format!("/{path}") };
    let base = url::Url::parse("relative://").ok()?;
    let u = base.join(&format!("//{path}")).ok()?;
    let mut out = u.path().to_owned();
    if include_search && let Some(q) = u.query() {
        out.push('?');
        out.push_str(q);
    }
    if include_hash && let Some(f) = u.fragment() {
        out.push('#');
        out.push_str(f);
    }
    Some(out)
}

/// `^https:\/\/+(?<host>[^/]+)\/?(?<path>.*)`
fn split_https(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix("https://")?.trim_start_matches('/');
    let end = rest.find('/').unwrap_or(rest.len());
    let host = &rest[..end];
    if host.is_empty() {
        return None;
    }
    let path = rest[end..].strip_prefix('/').unwrap_or(&rest[end..]);
    Some((host, path))
}

fn host_has_port(host: &str) -> bool {
    host.rsplit_once(':').is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn validate_url(
    token: &str,
    only_relative: bool,
    disallow_ports: bool,
    include_search: bool,
    include_hash: bool,
) -> Result<String, String> {
    if let Some((host, path)) = split_https(token) {
        if only_relative {
            return Err(format!("Only relative URLs are allowed. Skipping absolute URL {token}."));
        }
        if disallow_ports && host_has_port(host) {
            return Err(format!("Specifying ports is not supported. Skipping absolute URL {token}."));
        }
        let p = extract_pathname(path, include_search, include_hash)
            .ok_or_else(|| format!("Error parsing URL segment {token}. Skipping."))?;
        return Ok(format!("https://{host}{p}"));
    }
    let token = if !token.starts_with('/') && only_relative { format!("/{token}") } else { token.to_owned() };
    if token.starts_with('/') {
        return extract_pathname(&token, include_search, include_hash)
            .ok_or_else(|| format!("Error parsing URL segment {token}. Skipping."));
    }
    Err(if only_relative {
        "URLs should begin with a forward-slash.".into()
    } else {
        "URLs should either be relative (e.g. begin with a forward-slash), or use HTTPS (e.g. begin with \"https://\").".into()
    })
}

fn has_placeholder(s: &str) -> bool {
    let b = s.as_bytes();
    b.windows(2).any(|w| w[0] == b':' && w[1].is_ascii_alphabetic())
}

// ---- _headers (parseHeaders.ts, constructHeaders) ----

fn line_is_path(line: &str) -> bool {
    if line.starts_with('/') {
        return true;
    }
    // `[^\s]+://`
    line.find("://").is_some_and(|i| i > 0 && !line[..i].contains(char::is_whitespace))
}

fn wildcard_error(path: &str) -> bool {
    let stars = path.matches('*').count();
    let splat = path
        .match_indices(":splat")
        .any(|(i, m)| !path[i + m.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_'));
    stars > 1 || (stars > 0 && splat)
}

/// Parses a `_headers` file. Invalid lines are skipped, as on Cloudflare.
pub fn parse_headers(input: &str) -> Vec<HeaderRule> {
    struct Rule {
        path: String,
        set: Vec<(String, String)>,
        unset: Vec<String>,
    }
    let mut rules: Vec<HeaderRule> = Vec::new();
    let mut rule: Option<Rule> = None;
    let mut skip = false;
    let finish = |rule: Option<Rule>, rules: &mut Vec<HeaderRule>| {
        if let Some(r) = rule
            && (!r.set.is_empty() || !r.unset.is_empty())
        {
            rules.push(HeaderRule { path: r.path, set: r.set, unset: r.unset });
        }
    };
    for raw in input.split('\n') {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.len() > MAX_LINE_LENGTH {
            continue;
        }
        if line_is_path(line) {
            skip = false;
            if rules.len() >= MAX_HEADER_RULES {
                break;
            }
            finish(rule.take(), &mut rules);
            match validate_url(line, false, true, false, false) {
                Ok(path) if !wildcard_error(&path) => {
                    rule = Some(Rule { path, set: Vec::new(), unset: Vec::new() });
                }
                _ => skip = true,
            }
            continue;
        }
        if skip {
            continue;
        }
        let Some(r) = rule.as_mut() else { continue };
        let Some((name, value)) = line.split_once(':') else {
            if let Some(h) = line.strip_prefix("! ") {
                r.unset.push(h.to_owned());
            }
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name.is_empty() || name.contains(' ') || value.is_empty() {
            continue;
        }
        match r.set.iter_mut().find(|(n, _)| *n == name) {
            Some((_, v)) => *v = format!("{v}, {value}"),
            None => r.set.push((name, value.to_owned())),
        }
    }
    finish(rule, &mut rules);
    rules
}

// ---- _redirects (parseRedirects.ts, constructRedirects) ----

/// Parses a `_redirects` file. Invalid lines are skipped, as on Cloudflare.
pub fn parse_redirects(input: &str, html_handling: Option<&str>) -> Vec<Redirect> {
    let mut rules: Vec<Redirect> = Vec::new();
    let mut seen = BTreeSet::new();
    let (mut static_count, mut dynamic_count) = (0usize, 0usize);
    let mut can_static = true;
    for (i, raw) in input.split('\n').enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.len() > MAX_LINE_LENGTH {
            continue;
        }
        // `line.replace(/\s+#.*$/, "")`
        let line = match line
            .char_indices()
            .find(|&(j, c)| c == '#' && j > 0 && line[..j].ends_with(char::is_whitespace))
        {
            Some((j, _)) => line[..j].trim_end(),
            None => line,
        };
        let tokens: Vec<&str> = line.split_whitespace().collect();
        if tokens.len() < 2 || tokens.len() > 3 {
            continue;
        }
        let Ok(from) = validate_url(tokens[0], true, true, false, false) else { continue };
        let dynamic = from.contains('*') || has_placeholder(&from);
        if can_static && !dynamic {
            static_count += 1;
            if static_count > MAX_STATIC_REDIRECT_RULES {
                continue;
            }
        } else {
            dynamic_count += 1;
            can_static = false;
            if dynamic_count > MAX_DYNAMIC_REDIRECT_RULES {
                break;
            }
        }
        let Ok(to) = validate_url(tokens[1], false, false, true, true) else { continue };
        let Ok(status) = tokens.get(2).copied().unwrap_or("302").parse::<u16>() else { continue };
        if !PERMITTED_STATUS_CODES.contains(&status) {
            continue;
        }
        let relative = split_https(&to).is_none();
        let ends_index = |s: &str| s.ends_with("/index") || s.ends_with("/index.html");
        let wildcard_to_index = from.ends_with("/*") && ends_index(&to);
        let root_to_index = from.ends_with('/') && ends_index(&to);
        if relative && (wildcard_to_index || (root_to_index && html_handling != Some("none"))) {
            continue;
        }
        if !seen.insert(from.clone()) {
            continue;
        }
        if status == 200 && !relative {
            continue;
        }
        rules.push(Redirect { from, status, to, line_number: i + 1, is_static: false });
    }
    // constructRedirects: rules without splats or placeholders are static
    // until the first dynamic one.
    let mut can_static = true;
    for r in &mut rules {
        let dynamic = r.from.contains('*') || has_placeholder(&r.from);
        if !dynamic && can_static {
            r.is_static = true;
        } else {
            can_static = false;
        }
    }
    rules
}

// ---- run_worker_first rules (parseStaticRouting.ts) ----

pub fn parse_static_routing(input: &[String]) -> Result<StaticRouting, String> {
    if input.is_empty() {
        return Err("No `run_worker_first` rules were provided; must provide at least 1 rule.".into());
    }
    if input.len() > MAX_ROUTES_RULES {
        return Err(format!(
            "Too many `run_worker_first` rules were provided; {} rules provided exceeds max of {MAX_ROUTES_RULES}.",
            input.len()
        ));
    }
    let mut raw_asset = Vec::new();
    let mut asset = Vec::new();
    let mut user = Vec::new();
    let mut invalid = Vec::new();
    for rule in input {
        if let Some(r) = rule.strip_prefix('!') {
            if r.starts_with('/') {
                asset.push(r.to_owned());
                raw_asset.push(rule.clone());
            } else {
                invalid.push(format!("'{rule}': negative rules must start with '!/'"));
            }
        } else if rule.starts_with('/') {
            user.push(rule.clone());
        } else {
            invalid.push(format!("'{rule}': rules must start with '/' or '!/'"));
        }
    }
    if !asset.is_empty() && user.is_empty() {
        return Err(
            "Only negative `run_worker_first` rules were provided; must provide at least 1 non-negative rule"
                .into(),
        );
    }
    let check = |rules: &[String], invalid: &mut Vec<String>| {
        let mut seen = BTreeSet::new();
        for rule in rules {
            if rule.len() > MAX_ROUTES_RULE_LENGTH {
                invalid.push(format!(
                    "'{rule}': all rules must be less than {MAX_ROUTES_RULE_LENGTH} characters in length"
                ));
            }
            if seen.contains(rule) {
                invalid.push(format!("'{rule}': rule is a duplicate; rules must be unique"));
            }
            if let Some(prefix) = rule.strip_suffix('*') {
                for other in rules {
                    if other != rule && other.starts_with(prefix) {
                        invalid.push(format!("'{other}': rule '{rule}' makes it redundant"));
                    }
                }
            }
            seen.insert(rule.clone());
        }
    };
    check(&user, &mut invalid);
    check(&raw_asset, &mut invalid);
    if !invalid.is_empty() {
        return Err(format!("Invalid routes in `run_worker_first`:\n{}", invalid.join("\n")));
    }
    Ok(StaticRouting { user_worker: user, asset_worker: asset })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_sorted_by_path_hash() {
        let mut m = UploadManifest::new();
        m.insert("/index.html".into(), ManifestEntry { hash: "aa".repeat(16), size: 1 });
        m.insert("/app.js".into(), ManifestEntry { hash: "bb".repeat(16), size: 1 });
        let enc = encode_manifest(&m);
        assert_eq!(enc.len(), 20 + 2 * 40);
        let first = &enc[20..36];
        let second = &enc[60..76];
        assert!(first < second);
        use sha2::{Digest, Sha256};
        let idx = Sha256::digest(b"/index.html");
        let at = if first == &idx[..16] { 20 } else { 60 };
        assert_eq!(&enc[at + 16..at + 32], &[0xaa; 16]);
    }

    #[test]
    fn headers() {
        let rules = parse_headers(
            "# comment\n/*\n  X-Frame-Options: DENY\n  x-a: 1\n  X-A: 2\n/static/*\n  ! X-Frame-Options\n\
             nopath: x\nhttps://example.com:8080/x\n  a: b\n/empty\n/api/:id\n  Cache-Control: no-store",
        );
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].path, "/*");
        assert_eq!(
            rules[0].set,
            vec![("x-frame-options".into(), "DENY".into()), ("x-a".into(), "1, 2".into())]
        );
        assert_eq!(rules[1].unset, vec!["X-Frame-Options".to_owned()]);
        assert_eq!(rules[2].path, "/api/:id");
    }

    #[test]
    fn redirects() {
        let rules = parse_redirects(
            "/old /new 301\n/a /b # trailing comment\n/blog/* /posts/:splat\n/static /later\n\
             /x https://example.com/y?q=1\n/bad /z 404\n/a /dup\n/loop/* /index.html\n/p https://e.com/ 200",
            None,
        );
        let froms: Vec<_> = rules.iter().map(|r| (r.from.as_str(), r.status, r.is_static)).collect();
        assert_eq!(
            froms,
            [
                ("/old", 301, true),
                ("/a", 302, true),
                ("/blog/*", 302, false),
                ("/static", 302, false),
                ("/x", 302, false)
            ]
        );
        assert_eq!(rules[4].to, "https://example.com/y?q=1");
    }

    #[test]
    fn static_routing() {
        let r = parse_static_routing(&["/api/*".into(), "!/api/docs/*".into()]).unwrap();
        assert_eq!(r.user_worker, ["/api/*"]);
        assert_eq!(r.asset_worker, ["/api/docs/*"]);
        assert!(parse_static_routing(&["!/x".into()]).is_err());
        assert!(parse_static_routing(&["/a/*".into(), "/a/b".into()]).is_err());
        assert!(parse_static_routing(&["api".into()]).is_err());
    }

    #[test]
    fn configs_keep_rule_order() {
        let cfg = UploadConfig {
            html_handling: Some("auto-trailing-slash".into()),
            not_found_handling: Some("single-page-application".into()),
            run_worker_first: Some(json!(["/api/*"])),
            redirects: Some("/z/* /a 301\n/b/* /c".into()),
            headers: Some("/z\n  a: 1\n/a\n  b: 2".into()),
        };
        let a = ScriptAssets::from_upload("m".into(), 1, &cfg).unwrap();
        let c = a.asset_worker_config("2025-01-01", &[]);
        assert!(c.find("\"/z/*\"").unwrap() < c.find("\"/b/*\"").unwrap());
        assert!(c.find("\"/z\"").unwrap() < c.find("\"/a\":{\"set\"").unwrap());
        let v: Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["not_found_handling"], "single-page-application");
        assert_eq!(v["has_static_routing"], true);
        let r: Value = serde_json::from_str(&a.router_config(true)).unwrap();
        assert_eq!(r["static_routing"]["user_worker"][0], "/api/*");
        assert!(
            ScriptAssets::from_upload(
                "m".into(),
                1,
                &UploadConfig { html_handling: Some("bogus".into()), ..Default::default() }
            )
            .is_err()
        );
    }
}
