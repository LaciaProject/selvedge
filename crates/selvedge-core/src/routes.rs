//! Worker route patterns (`example.com/api/*`), matched as Cloudflare does:
//! the host may start with `*` (any prefix, e.g. `*.example.com`), the path
//! may end with `*` (any suffix), and the query string is ignored. When
//! several patterns match, the most specific wins: an exact host over a
//! wildcard one, then the longer pattern.

/// Checks a pattern's form and that its host is `zone` or under it.
pub fn validate(pattern: &str, zone: &str) -> Result<(), String> {
    let (host, path) =
        split(pattern).ok_or_else(|| format!("route `{pattern}` must look like `host/path`"))?;
    if path.contains('*') && !path.ends_with('*') || path[..path.len().saturating_sub(1)].contains('*') {
        return Err(format!("route `{pattern}`: `*` may only end the path"));
    }
    let bare = host.trim_start_matches('*').trim_start_matches('.');
    if host[1..].contains('*') || bare.is_empty() {
        return Err(format!("route `{pattern}`: `*` may only start the host"));
    }
    if !(bare == zone
        || bare.ends_with(&format!(".{zone}"))
        || (host.starts_with('*') && zone.ends_with(bare)))
    {
        return Err(format!("route `{pattern}` is not in zone `{zone}`"));
    }
    Ok(())
}

fn split(pattern: &str) -> Option<(&str, &str)> {
    let p = pattern.strip_prefix("https://").or_else(|| pattern.strip_prefix("http://")).unwrap_or(pattern);
    let i = p.find('/')?;
    let (host, path) = p.split_at(i);
    (!host.is_empty()).then_some((host, path))
}

/// How specifically `pattern` matches a request; `None` if it does not.
pub fn score(pattern: &str, host: &str, path: &str) -> Option<(bool, usize)> {
    let (ph, pp) = split(pattern)?;
    let host_ok = match ph.strip_prefix('*') {
        Some(suffix) => host.ends_with(suffix),
        None => ph.eq_ignore_ascii_case(host),
    };
    let path_ok = match pp.strip_suffix('*') {
        Some(prefix) => path.starts_with(prefix),
        None => path == pp,
    };
    (host_ok && path_ok).then_some((!ph.starts_with('*'), pattern.len()))
}

/// The best match among `patterns` (by index).
pub fn best<'a, I: IntoIterator<Item = &'a str>>(patterns: I, host: &str, path: &str) -> Option<usize> {
    patterns
        .into_iter()
        .enumerate()
        .filter_map(|(i, p)| score(p, host, path).map(|s| (s, i)))
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(_, i)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        assert!(validate("example.com/*", "example.com").is_ok());
        assert!(validate("api.example.com/v1/*", "example.com").is_ok());
        assert!(validate("*.example.com/*", "example.com").is_ok());
        assert!(validate("*example.com/*", "example.com").is_ok());
        assert!(validate("other.com/*", "example.com").is_err());
        assert!(validate("example.com", "example.com").is_err());
        assert!(validate("example.com/*/x", "example.com").is_err());
        assert!(validate("a.*.example.com/*", "example.com").is_err());
    }

    #[test]
    fn matching() {
        let p = ["example.com/*", "example.com/api/*", "*.example.com/*", "example.com/exact"];
        assert_eq!(best(p, "example.com", "/"), Some(0));
        assert_eq!(best(p, "example.com", "/api/x"), Some(1));
        assert_eq!(best(p, "a.example.com", "/api/x"), Some(2));
        assert_eq!(best(p, "example.com", "/exact"), Some(3));
        assert_eq!(best(p, "notexample.com", "/"), None);
        assert_eq!(best(["*example.com/*"], "notexample.com", "/"), Some(0));
    }
}
