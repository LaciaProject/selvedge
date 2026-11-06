//! Content-addressed blobs (module bytes) in an S3-compatible bucket, a local
//! directory, or memory.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use object_store::path::Path;
use object_store::{ObjectStore, PutPayload};

#[derive(Clone)]
pub struct Blobs {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
}

impl Blobs {
    /// Opens a store from a URL: `s3://bucket/prefix`, `file:///dir` or `memory://`.
    /// S3 settings come from `options` (`aws_endpoint`, `aws_access_key_id`, ...)
    /// and fall back to the standard `AWS_*` environment variables.
    pub fn open(url: &str, options: &[(String, String)]) -> Result<Self> {
        let parsed = url::Url::parse(url).with_context(|| format!("parsing blob store URL `{url}`"))?;
        let (store, prefix): (Arc<dyn ObjectStore>, Path) = match parsed.scheme() {
            "memory" => (Arc::new(object_store::memory::InMemory::new()), Path::default()),
            "file" => {
                let dir = parsed.path();
                std::fs::create_dir_all(dir)?;
                (Arc::new(object_store::local::LocalFileSystem::new_with_prefix(dir)?), Path::default())
            }
            "s3" => {
                let mut builder =
                    object_store::aws::AmazonS3Builder::from_env().with_url(url).with_allow_http(true);
                for (k, v) in options {
                    builder = builder.with_config(k.parse()?, v);
                }
                let prefix = Path::from(parsed.path().trim_matches('/'));
                (Arc::new(builder.build()?), prefix)
            }
            other => anyhow::bail!("unsupported blob store scheme `{other}`"),
        };
        Ok(Self { store, prefix })
    }

    fn path(&self, sha256: &str) -> Path {
        self.prefix.child("blobs").child(sha256)
    }

    /// Stores `bytes` under its SHA-256 and returns the hash.
    pub async fn put(&self, bytes: Bytes) -> Result<String> {
        let sha = selvedge_core::sha256_hex(&bytes);
        let path = self.path(&sha);
        // Content-addressed: an existing object already has these bytes.
        if self.store.head(&path).await.is_err() {
            self.store.put(&path, PutPayload::from(bytes)).await?;
        }
        Ok(sha)
    }

    pub async fn get(&self, sha256: &str) -> Result<Option<Bytes>> {
        anyhow::ensure!(
            sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid blob hash"
        );
        match self.store.get(&self.path(sha256)).await {
            Ok(r) => Ok(Some(r.bytes().await?)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        for url in ["memory://".to_owned(), format!("file://{}", dir.path().display())] {
            let blobs = Blobs::open(&url, &[]).unwrap();
            let sha = blobs.put(Bytes::from_static(b"hello")).await.unwrap();
            assert_eq!(sha, selvedge_core::sha256_hex(b"hello"));
            assert_eq!(blobs.put(Bytes::from_static(b"hello")).await.unwrap(), sha);
            assert_eq!(blobs.get(&sha).await.unwrap().unwrap(), Bytes::from_static(b"hello"));
            assert!(blobs.get(&"0".repeat(64)).await.unwrap().is_none());
        }
    }
}
