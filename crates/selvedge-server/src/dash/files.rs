//! The dashboard's built files (`dash/dist`). Paths the files do not have are
//! the single-page app's own routes and get `index.html`.

use std::path::{Component, Path, PathBuf};

use axum::extract::{Request, State as AxState};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::State;

pub async fn serve(AxState(d): AxState<State>, req: Request) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(dir) = &d.cfg.static_dir else {
        return (StatusCode::NOT_FOUND, "The dashboard's files are not installed (--dash-dir).")
            .into_response();
    };
    let path = req.uri().path();
    // API-like paths that are not routed must not turn into the app.
    if path.starts_with("/auth/") || path.starts_with("/internal/") || path.starts_with("/registry/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some(file) = resolve(dir, path) {
        if let Ok(bytes) = tokio::fs::read(&file).await {
            let immutable = path.starts_with("/assets/");
            return file_response(&file, bytes, immutable);
        }
    }
    // A missing file with an extension is a missing file, not a page.
    let last = path.rsplit('/').next().unwrap_or_default();
    if last.contains('.') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let index = dir.join("index.html");
    match tokio::fs::read(&index).await {
        Ok(bytes) => file_response(&index, bytes, false),
        Err(_) => (StatusCode::NOT_FOUND, "The dashboard's index.html is missing.").into_response(),
    }
}

/// The file a URL path names under `dir`, refusing anything that would step
/// outside it.
fn resolve(dir: &Path, url_path: &str) -> Option<PathBuf> {
    let decoded = percent_encoding::percent_decode_str(url_path).decode_utf8().ok()?;
    let rel = Path::new(decoded.trim_start_matches('/'));
    if rel.as_os_str().is_empty() || decoded.contains('\\') || decoded.contains('\0') {
        return None;
    }
    if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    let file = dir.join(rel);
    file.is_file().then_some(file)
}

fn file_response(file: &Path, bytes: Vec<u8>, immutable: bool) -> Response {
    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or_default();
    let content_type = match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    };
    // Vite names assets by their content: they never change. Everything
    // else, index.html above all, must be checked again.
    let cache = if immutable { "public, max-age=31536000, immutable" } else { "no-cache" };
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn stays_inside_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/app.js"), "x").unwrap();
        std::fs::write(dir.path().join("index.html"), "x").unwrap();
        assert!(resolve(dir.path(), "/assets/app.js").is_some());
        assert!(resolve(dir.path(), "/index.html").is_some());
        for bad in [
            "/",
            "/assets",
            "/../etc/passwd",
            "/assets/../../etc/passwd",
            "/%2e%2e/etc/passwd",
            "/a\\b",
            "/nope.js",
        ] {
            assert!(resolve(dir.path(), bad).is_none(), "{bad}");
        }
    }
}
