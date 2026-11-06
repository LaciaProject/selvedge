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

    fn raw(&self, path: &str) -> Path {
        let mut p = self.prefix.clone();
        for part in path.split('/') {
            p = p.child(part);
        }
        p
    }

    /// A streaming writer for a blob at `path` (relative to the prefix). Call
    /// `shutdown()` to finish the upload.
    pub fn writer(&self, path: &str) -> object_store::buffered::BufWriter {
        object_store::buffered::BufWriter::new(self.store.clone(), self.raw(path))
    }

    /// Bytes `start..end` of the blob at `path`.
    pub async fn read_range(
        &self,
        path: &str,
        start: u64,
        end: u64,
    ) -> Result<futures::stream::BoxStream<'static, object_store::Result<Bytes>>> {
        let opts = object_store::GetOptions {
            range: Some(object_store::GetRange::Bounded(start..end)),
            ..Default::default()
        };
        Ok(self.store.get_opts(&self.raw(path), opts).await?.into_stream())
    }

    /// Deletes the blob at `path`; a missing blob is not an error.
    pub async fn delete(&self, path: &str) -> Result<()> {
        match self.store.delete(&self.raw(path)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
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
