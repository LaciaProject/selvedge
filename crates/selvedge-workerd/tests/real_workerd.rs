//! Runs the official workerd binary on a rendered config. Skipped unless
//! `SELVEDGE_WORKERD` names the binary.

use std::path::PathBuf;
use std::time::Duration;

use selvedge_core::assets::{ManifestEntry, ScriptAssets, UploadConfig, UploadManifest, encode_manifest};
use selvedge_core::{AccountBundle, Binding, BundleScript, HexId, Module, ModuleKind};
use selvedge_workerd::{RenderOptions, WorkerdProcess, render, workerd_max_date};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn workerd() -> Option<PathBuf> {
    std::env::var_os("SELVEDGE_WORKERD").map(PathBuf::from)
}

async fn get(socket: &std::path::Path, path: &str) -> String {
    let mut s = tokio::net::UnixStream::connect(socket).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn serves_rendered_scripts() {
    let Some(bin) = workerd() else {
        eprintln!("SELVEDGE_WORKERD not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let main = br#"
import { greet } from "./lib.js";
export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    if (url.pathname === "/other") return env.OTHER.fetch("http://x/");
    if (url.pathname === "/missing") return env.MISSING.fetch("http://x/");
    if (url.pathname === "/d1") {
      await env.DB.exec("CREATE TABLE IF NOT EXISTS t (v INTEGER)");
      await env.DB.prepare("INSERT INTO t VALUES (?)").bind(41).run();
      return new Response(String((await env.DB.prepare("SELECT sum(v) + 1 AS s FROM t").first("s"))));
    }
    return new Response(greet(env.NAME) + " " + env.CONF.n + " " + env.SECRET.length);
  }
};"#;
    let lib = b"export const greet = (n) => `hello ${n}`;";
    let other = b"export default { fetch() { return new Response('from other'); } };";
    let blobs = [main.to_vec(), lib.to_vec(), other.to_vec()];
    let module = |name: &str, bytes: &[u8]| Module {
        name: name.into(),
        kind: ModuleKind::EsModule,
        sha256: selvedge_core::sha256_hex(bytes),
    };
    let bundle = AccountBundle {
        account_id: HexId::random(),
        generation: 1,
        scripts: vec![
            BundleScript {
                name: "main".into(),
                version_id: selvedge_core::new_version_id(),
                main_module: "index.js".into(),
                compatibility_date: "2025-01-01".into(),
                compatibility_flags: vec![],
                modules: vec![module("lib.js", lib), module("index.js", main)],
                bindings: vec![
                    Binding::PlainText { name: "NAME".into(), text: "wor\"ld".into() },
                    Binding::Json { name: "CONF".into(), json: serde_json::json!({"n": 7}) },
                    Binding::SecretText { name: "SECRET".into(), text: Some("abc".into()) },
                    Binding::Service { name: "OTHER".into(), service: "other".into(), entrypoint: None },
                    Binding::Service { name: "MISSING".into(), service: "gone".into(), entrypoint: None },
                    Binding::D1 { name: "DB".into(), id: "db-1".into() },
                ],
                assets: None,
                canary: None,
            },
            BundleScript {
                name: "other".into(),
                version_id: selvedge_core::new_version_id(),
                main_module: "o.js".into(),
                compatibility_date: "2025-01-01".into(),
                compatibility_flags: vec![],
                modules: vec![module("o.js", other)],
                bindings: vec![],
                assets: None,
                canary: None,
            },
        ],
        d1_databases: vec!["db-1".into()],
        workflows: vec![],
    };
    let opts = RenderOptions {
        dir: dir.path().join("cfg"),
        socket_dir: dir.path().join("s"),
        binding_socket: dir.path().join("b.sock"),
        max_compatibility_date: workerd_max_date(&bin).await.unwrap(),
        do_storage_dir: dir.path().join("do"),
        cluster: None,
    };
    let src = |sha: &str| {
        blobs
            .iter()
            .find(|b| selvedge_core::sha256_hex(b) == sha)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no blob {sha}"))
    };
    let rendered = render(&bundle, &opts, &src).unwrap();
    let proc = WorkerdProcess::start(&bin, &rendered, &[], "test", Duration::from_secs(20)).await.unwrap();
    let main_sock = proc.socket("main").unwrap().to_owned();
    let body = get(&main_sock, "/").await;
    assert!(body.ends_with("hello wor\"ld 7 3"), "{body}");
    assert!(get(&main_sock, "/other").await.ends_with("from other"));
    let missing = get(&main_sock, "/missing").await;
    assert!(missing.starts_with("HTTP/1.1 503"), "{missing}");
    let d1 = get(&main_sock, "/d1").await;
    assert!(d1.ends_with("\r\n\r\n42") || d1.ends_with("42"), "{d1}");
    proc.stop(Duration::from_secs(5)).await;
}

/// Answers asset reads on the binding socket the way the data service does.
fn serve_assets(path: &std::path::Path, files: Vec<(String, &'static str, &'static str)>) {
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    let files = std::sync::Arc::new(files);
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let files = files.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = s.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                assert!(head.to_ascii_lowercase().contains("x-selvedge-assets: 1"), "{head}");
                let target = head.split_whitespace().nth(1).unwrap_or_default();
                let hash = target.trim_start_matches('/').split('?').next().unwrap_or_default();
                let resp = match files.iter().find(|(h, _, _)| h == hash) {
                    Some((_, body, ct)) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nCF-KV-Metadata: {{\"contentType\":\"{ct}\"}}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len()
                    ),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
                };
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
}

#[tokio::test]
async fn serves_static_assets() {
    let Some(bin) = workerd() else {
        eprintln!("SELVEDGE_WORKERD not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let (home, about) = ("a".repeat(32), "b".repeat(32));
    let mut manifest = UploadManifest::new();
    manifest.insert("/index.html".into(), ManifestEntry { hash: home.clone(), size: 13 });
    manifest.insert("/about/index.html".into(), ManifestEntry { hash: about.clone(), size: 14 });
    let encoded = encode_manifest(&manifest);
    let manifest_sha = selvedge_core::sha256_hex(&encoded);
    let cfg = UploadConfig {
        headers: Some("/*\n  X-Site: selvedge".into()),
        redirects: Some("/old /about/ 301".into()),
        ..Default::default()
    };
    let assets = ScriptAssets::from_upload(manifest_sha.clone(), 2, &cfg).unwrap();
    let app = br#"
export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    if (url.pathname === "/api") return new Response("api " + await (await env.ASSETS.fetch(new URL("/", req.url))).text());
    if (url.pathname === "/site") return env.SITE.fetch(new URL("/about/", req.url));
    return new Response("worker " + url.pathname);
  }
};"#;
    let app_module =
        Module { name: "app.js".into(), kind: ModuleKind::EsModule, sha256: selvedge_core::sha256_hex(app) };
    let script = |name: &str, modules: Vec<Module>, bindings: Vec<Binding>| BundleScript {
        name: name.into(),
        version_id: selvedge_core::new_version_id(),
        main_module: if modules.is_empty() { String::new() } else { "app.js".into() },
        compatibility_date: "2025-06-01".into(),
        compatibility_flags: vec![],
        modules,
        bindings,
        assets: Some(assets.clone()),
        canary: None,
    };
    let bundle = AccountBundle {
        account_id: HexId::random(),
        generation: 1,
        scripts: vec![
            script(
                "app",
                vec![app_module],
                vec![
                    Binding::Assets { name: "ASSETS".into() },
                    Binding::Service { name: "SITE".into(), service: "site".into(), entrypoint: None },
                ],
            ),
            script("site", vec![], vec![]),
        ],
        d1_databases: vec![],
        workflows: vec![],
    };
    serve_assets(
        &dir.path().join("b.sock"),
        vec![(home, "<h1>home</h1>", "text/html"), (about, "<h1>about</h1>", "text/html")],
    );
    let opts = RenderOptions {
        dir: dir.path().join("cfg"),
        socket_dir: dir.path().join("s"),
        binding_socket: dir.path().join("b.sock"),
        max_compatibility_date: workerd_max_date(&bin).await.unwrap(),
        do_storage_dir: dir.path().join("do"),
        cluster: None,
    };
    let blobs = [app.to_vec(), encoded];
    let src = |sha: &str| {
        blobs
            .iter()
            .find(|b| selvedge_core::sha256_hex(b) == sha)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no blob {sha}"))
    };
    let rendered = render(&bundle, &opts, &src).unwrap();
    let proc = WorkerdProcess::start(&bin, &rendered, &[], "assets", Duration::from_secs(20)).await.unwrap();
    let app_sock = proc.socket("app").unwrap().to_owned();
    let site_sock = proc.socket("site").unwrap().to_owned();

    let home = get(&app_sock, "/").await;
    assert!(home.starts_with("HTTP/1.1 200") && home.ends_with("<h1>home</h1>"), "{home}");
    assert!(home.to_ascii_lowercase().contains("x-site: selvedge"), "{home}");
    assert!(home.to_ascii_lowercase().contains("content-type: text/html"), "{home}");
    let api = get(&app_sock, "/api").await;
    assert!(api.ends_with("api <h1>home</h1>"), "{api}");
    let fallthrough = get(&app_sock, "/nope").await;
    assert!(fallthrough.ends_with("worker /nope"), "{fallthrough}");
    let via_binding = get(&app_sock, "/site").await;
    assert!(via_binding.ends_with("<h1>about</h1>"), "{via_binding}");

    let slash = get(&site_sock, "/about").await;
    assert!(slash.starts_with("HTTP/1.1 307") && slash.contains("/about/"), "{slash}");
    let old = get(&site_sock, "/old").await;
    assert!(old.starts_with("HTTP/1.1 301"), "{old}");
    let missing = get(&site_sock, "/missing").await;
    assert!(missing.starts_with("HTTP/1.1 404"), "{missing}");
    proc.stop(Duration::from_secs(5)).await;
}
