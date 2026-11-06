//! Runs the official workerd binary on a rendered config. Skipped unless
//! `SELVEDGE_WORKERD` names the binary.

use std::path::PathBuf;
use std::time::Duration;

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
                ],
            },
            BundleScript {
                name: "other".into(),
                version_id: selvedge_core::new_version_id(),
                main_module: "o.js".into(),
                compatibility_date: "2025-01-01".into(),
                compatibility_flags: vec![],
                modules: vec![module("o.js", other)],
                bindings: vec![],
            },
        ],
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
    let proc = WorkerdProcess::start(&bin, &rendered, "test", Duration::from_secs(20)).await.unwrap();
    let main_sock = proc.socket("main").unwrap().to_owned();
    let body = get(&main_sock, "/").await;
    assert!(body.ends_with("hello wor\"ld 7 3"), "{body}");
    assert!(get(&main_sock, "/other").await.ends_with("from other"));
    let missing = get(&main_sock, "/missing").await;
    assert!(missing.starts_with("HTTP/1.1 503"), "{missing}");
    proc.stop(Duration::from_secs(5)).await;
}
