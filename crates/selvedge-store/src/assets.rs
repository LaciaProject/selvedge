//! Workers Static Assets: uploaded files and upload sessions.

use std::collections::BTreeSet;

use anyhow::Result;
use selvedge_core::AccountId;
use selvedge_core::assets::UploadManifest;

use crate::Db;

/// Where a file uploaded before asset paths were recorded lives.
pub fn legacy_asset_path(account: &AccountId, hash: &str) -> String {
    format!("assets/{account}/{hash}")
}

/// A fresh blob path for one upload of an asset file.
pub fn new_asset_path(account: &AccountId, hash: &str) -> String {
    format!("assets/{account}/{hash}/{}", selvedge_core::HexId::random())
}

/// How long an upload session (and its completion token) stays valid.
pub const SESSION_TTL_SECS: i64 = 3600;

/// An upload session found by its upload token.
#[derive(Debug, Clone)]
pub struct AssetSession {
    pub id: String,
    pub script: String,
    pub manifest: UploadManifest,
    pub pending: BTreeSet<String>,
}

/// One uploaded file.
#[derive(Debug, Clone)]
pub struct AssetFile {
    pub hash: String,
    pub content_type: Option<String>,
    pub size: i64,
    /// Its blob's path.
    pub path: String,
}

impl Db {
    /// The hashes among `hashes` that the account has not uploaded yet.
    pub async fn missing_assets(&self, account: &AccountId, hashes: &[String]) -> Result<BTreeSet<String>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT hash FROM asset_files WHERE account_id = $1 AND hash = ANY($2)",
                &[&account.as_str(), &hashes],
            )
            .await?;
        let have: BTreeSet<String> = rows.iter().map(|r| r.get(0)).collect();
        Ok(hashes.iter().filter(|h| !have.contains(*h)).cloned().collect())
    }

    /// Starts an upload session. With nothing pending, the completion token is
    /// stored at once.
    pub async fn create_asset_session(
        &self,
        account: &AccountId,
        script: &str,
        manifest: &UploadManifest,
        pending: &BTreeSet<String>,
        upload_token_sha256: &str,
        completion_token_sha256: Option<&str>,
    ) -> Result<String> {
        let client = self.client().await?;
        client.execute("DELETE FROM asset_upload_sessions WHERE expires_at < now()", &[]).await?;
        let id = selvedge_core::new_version_id();
        client
            .execute(
                "INSERT INTO asset_upload_sessions
                   (id, account_id, script_name, manifest, pending, upload_token_sha256, completion_token_sha256,
                    expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, now() + make_interval(secs => $8))",
                &[
                    &id,
                    &account.as_str(),
                    &script,
                    &serde_json::to_value(manifest)?,
                    &serde_json::to_value(pending)?,
                    &upload_token_sha256,
                    &completion_token_sha256,
                    &(SESSION_TTL_SECS as f64),
                ],
            )
            .await?;
        Ok(id)
    }

    /// The live session an upload token belongs to.
    pub async fn asset_session_by_upload(
        &self,
        account: &AccountId,
        upload_token_sha256: &str,
    ) -> Result<Option<AssetSession>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT id, script_name, manifest, pending FROM asset_upload_sessions
                 WHERE account_id = $1 AND upload_token_sha256 = $2 AND expires_at > now()",
                &[&account.as_str(), &upload_token_sha256],
            )
            .await?;
        row.map(|r| {
            Ok(AssetSession {
                id: r.get(0),
                script: r.get(1),
                manifest: serde_json::from_value(r.get(2))?,
                pending: serde_json::from_value(r.get(3))?,
            })
        })
        .transpose()
    }

    /// Records uploaded files. Returns the paths of those that were already
    /// recorded (a concurrent upload of the same hash won), whose new blobs
    /// the caller should delete.
    pub async fn put_asset_files(&self, account: &AccountId, files: &[AssetFile]) -> Result<Vec<String>> {
        let client = self.client().await?;
        let mut duplicates = Vec::new();
        for f in files {
            let n = client
                .execute(
                    "INSERT INTO asset_files (account_id, hash, content_type, size, path) VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (account_id, hash) DO NOTHING",
                    &[&account.as_str(), &f.hash, &f.content_type, &f.size, &f.path],
                )
                .await?;
            if n == 0 {
                duplicates.push(f.path.clone());
            }
        }
        Ok(duplicates)
    }

    /// Records the files a version manifest references, so they are kept.
    pub async fn record_asset_manifest(
        &self,
        account: &AccountId,
        manifest: &str,
        hashes: &[String],
    ) -> Result<()> {
        self.client()
            .await?
            .execute(
                "INSERT INTO asset_manifest_entries (account_id, manifest, hash)
                 SELECT $1, $2, h FROM unnest($3::text[]) AS h
                 ON CONFLICT (account_id, manifest, hash) DO UPDATE SET created_at = now()",
                &[&account.as_str(), &manifest, &hashes],
            )
            .await?;
        Ok(())
    }

    /// Deletes up to `limit` asset files that no stored version and no live
    /// upload session references, and that were not uploaded or referenced
    /// in the last `grace_secs`. Returns their blob paths, for the caller to
    /// delete. Rows go first, so a blob is never deleted while a row names it.
    pub async fn collect_asset_files(&self, grace_secs: i64, limit: i64) -> Result<Vec<String>> {
        let client = self.client().await?;
        let grace = grace_secs as f64;
        // Manifest entries of manifests no version uses any more.
        client
            .execute(
                "DELETE FROM asset_manifest_entries e
                 WHERE e.created_at < now() - make_interval(secs => $1)
                   AND NOT EXISTS (SELECT 1 FROM script_versions v
                                   WHERE v.account_id = e.account_id AND v.assets->>'manifest' = e.manifest)",
                &[&grace],
            )
            .await?;
        let rows = client
            .query(
                "WITH doomed AS (
                   SELECT f.account_id, f.hash FROM asset_files f
                   WHERE f.path IS NOT NULL
                     AND f.created_at < now() - make_interval(secs => $1)
                     AND NOT EXISTS (SELECT 1 FROM asset_manifest_entries e
                                     WHERE e.account_id = f.account_id AND e.hash = f.hash)
                     AND NOT EXISTS (SELECT 1 FROM asset_upload_sessions s
                                     WHERE s.account_id = f.account_id AND s.expires_at > now()
                                       AND jsonb_path_exists(s.manifest, '$.*.hash ? (@ == $h)',
                                                             jsonb_build_object('h', f.hash)))
                   LIMIT $2
                   FOR UPDATE OF f SKIP LOCKED
                 )
                 DELETE FROM asset_files f USING doomed d
                 WHERE f.account_id = d.account_id AND f.hash = d.hash
                 RETURNING f.path",
                &[&grace, &limit],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// Marks hashes of a session uploaded. Returns `true` exactly once: for the
    /// call that uploads the last pending file, which then stores the
    /// completion token.
    pub async fn finish_asset_uploads(
        &self,
        session: &str,
        uploaded: &[String],
        completion_token_sha256: &str,
    ) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let Some(row) = tx
            .query_opt(
                "SELECT pending, completion_token_sha256 IS NOT NULL FROM asset_upload_sessions
                 WHERE id = $1 FOR UPDATE",
                &[&session],
            )
            .await?
        else {
            return Ok(false);
        };
        let mut pending: BTreeSet<String> = serde_json::from_value(row.get(0))?;
        let done: bool = row.get(1);
        for h in uploaded {
            pending.remove(h);
        }
        let completes = pending.is_empty() && !done;
        tx.execute(
            "UPDATE asset_upload_sessions SET pending = $2,
               completion_token_sha256 = CASE WHEN $3 THEN $4 ELSE completion_token_sha256 END
             WHERE id = $1",
            &[&session, &serde_json::to_value(&pending)?, &completes, &completion_token_sha256],
        )
        .await?;
        tx.commit().await?;
        Ok(completes)
    }

    /// The manifest of a completed session, by its completion token. The
    /// session is used up: one completion token makes one version.
    pub async fn take_asset_manifest(
        &self,
        account: &AccountId,
        completion_token_sha256: &str,
    ) -> Result<Option<UploadManifest>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "DELETE FROM asset_upload_sessions
                 WHERE account_id = $1 AND completion_token_sha256 = $2 AND expires_at > now()
                 RETURNING manifest",
                &[&account.as_str(), &completion_token_sha256],
            )
            .await?;
        row.map(|r| Ok(serde_json::from_value(r.get(0))?)).transpose()
    }

    /// One uploaded file of an account.
    pub async fn asset_file(&self, account: &AccountId, hash: &str) -> Result<Option<AssetFile>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT content_type, size, path FROM asset_files WHERE account_id = $1 AND hash = $2",
                &[&account.as_str(), &hash],
            )
            .await?;
        Ok(row.map(|r| AssetFile {
            hash: hash.to_owned(),
            content_type: r.get(0),
            size: r.get(1),
            path: r.get::<_, Option<String>>(2).unwrap_or_else(|| legacy_asset_path(account, hash)),
        }))
    }
}
