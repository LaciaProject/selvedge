//! Workers Static Assets: uploaded files and upload sessions.

use std::collections::BTreeSet;

use anyhow::Result;
use selvedge_core::AccountId;
use selvedge_core::assets::UploadManifest;

use crate::Db;

/// Where an account's asset file lives in the blob store.
pub fn asset_path(account: &AccountId, hash: &str) -> String {
    format!("assets/{account}/{hash}")
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

    /// Records uploaded files.
    pub async fn put_asset_files(&self, account: &AccountId, files: &[AssetFile]) -> Result<()> {
        let client = self.client().await?;
        for f in files {
            client
                .execute(
                    "INSERT INTO asset_files (account_id, hash, content_type, size) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (account_id, hash) DO UPDATE SET content_type = excluded.content_type",
                    &[&account.as_str(), &f.hash, &f.content_type, &f.size],
                )
                .await?;
        }
        Ok(())
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

    /// The manifest of a completed session, by its completion token.
    pub async fn asset_manifest_by_completion(
        &self,
        account: &AccountId,
        completion_token_sha256: &str,
    ) -> Result<Option<UploadManifest>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT manifest FROM asset_upload_sessions
                 WHERE account_id = $1 AND completion_token_sha256 = $2 AND expires_at > now()",
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
                "SELECT content_type, size FROM asset_files WHERE account_id = $1 AND hash = $2",
                &[&account.as_str(), &hash],
            )
            .await?;
        Ok(row.map(|r| AssetFile { hash: hash.to_owned(), content_type: r.get(0), size: r.get(1) }))
    }
}
