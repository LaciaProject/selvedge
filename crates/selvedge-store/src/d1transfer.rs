//! Files moving to and from D1 databases through the API.

use anyhow::Result;
use selvedge_core::AccountId;

use crate::Db;

/// How long a transfer URL stays valid.
pub const TRANSFER_TTL_SECS: i64 = 3600;

#[derive(Debug, Clone)]
pub struct Transfer {
    pub account: AccountId,
    pub database_id: String,
    pub kind: String,
    pub etag: Option<String>,
    pub blob_path: Option<String>,
}

impl Db {
    pub async fn create_d1_transfer(
        &self,
        token_sha256: &str,
        account: &AccountId,
        database_id: &str,
        kind: &str,
        etag: Option<&str>,
        blob_path: Option<&str>,
    ) -> Result<()> {
        self.client()
            .await?
            .execute(
                "INSERT INTO d1_transfers (token_sha256, account_id, database_id, kind, etag, blob_path, expires_at)
                 VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
                &[
                    &token_sha256,
                    &account.as_str(),
                    &database_id,
                    &kind,
                    &etag,
                    &blob_path,
                    &(TRANSFER_TTL_SECS as f64),
                ],
            )
            .await?;
        Ok(())
    }

    /// A live transfer by its token's SHA-256.
    pub async fn d1_transfer(&self, token_sha256: &str) -> Result<Option<Transfer>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT account_id, database_id, kind, etag, blob_path FROM d1_transfers
                 WHERE token_sha256 = $1 AND expires_at > now()",
                &[&token_sha256],
            )
            .await?;
        row.map(|r| {
            Ok(Transfer {
                account: r.get::<_, String>(0).parse()?,
                database_id: r.get(1),
                kind: r.get(2),
                etag: r.get(3),
                blob_path: r.get(4),
            })
        })
        .transpose()
    }

    /// Records an import's uploaded file; `false` if the transfer is gone
    /// or already has one.
    pub async fn set_d1_transfer_blob(&self, token_sha256: &str, blob_path: &str) -> Result<bool> {
        Ok(self
            .client()
            .await?
            .execute(
                "UPDATE d1_transfers SET blob_path = $2
                 WHERE token_sha256 = $1 AND kind = 'import' AND blob_path IS NULL AND expires_at > now()",
                &[&token_sha256, &blob_path],
            )
            .await?
            == 1)
    }

    /// Ends a transfer; returns its blob path, for the caller to delete.
    pub async fn take_d1_transfer(&self, token_sha256: &str) -> Result<Option<String>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "DELETE FROM d1_transfers WHERE token_sha256 = $1 RETURNING blob_path",
                &[&token_sha256],
            )
            .await?;
        Ok(row.and_then(|r| r.get(0)))
    }

    /// Ends expired transfers; returns their blob paths, for the caller to
    /// delete.
    pub async fn take_expired_d1_transfers(&self) -> Result<Vec<String>> {
        let rows = self
            .client()
            .await?
            .query("DELETE FROM d1_transfers WHERE expires_at <= now() RETURNING blob_path", &[])
            .await?;
        Ok(rows.iter().filter_map(|r| r.get::<_, Option<String>>(0)).collect())
    }
}
