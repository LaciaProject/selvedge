//! R2 metadata. Every mutation that can race (conditional puts, multipart
//! completion) runs in one transaction with the object row locked, so R2's
//! strong consistency holds across any number of data service replicas.

use anyhow::Result;
use selvedge_core::{AccountId, HexId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Db;

/// One blob of an object's bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct R2Part {
    pub path: String,
    pub size: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct R2Object {
    pub key: String,
    pub version: String,
    pub size: i64,
    pub etag: String,
    pub uploaded_ms: i64,
    /// `R2HttpFields` as workerd encodes it (camelCase).
    pub http_fields: Value,
    /// A list of `{k, v}` records.
    pub custom_fields: Value,
    /// `{"0": md5 hex, "1": sha1 hex, ...}`, as workerd encodes `R2Checksums`.
    pub checksums: Value,
    pub storage_class: String,
    pub parts: Vec<R2Part>,
}

#[derive(Clone, Debug)]
pub struct R2Upload {
    pub id: String,
    pub key: String,
    pub http_fields: Value,
    pub custom_fields: Value,
    pub storage_class: String,
}

#[derive(Clone, Debug)]
pub struct R2UploadedPart {
    pub part: i32,
    pub etag: String,
    pub size: i64,
    pub path: String,
}

pub enum BucketDelete {
    Deleted,
    NotFound,
    NotEmpty,
}

pub enum PutOutcome {
    Stored,
    /// The condition did not hold; nothing changed.
    PreconditionFailed,
}

const OBJECT_COLUMNS: &str =
    "key, version, size, etag, uploaded_ms, http_fields, custom_fields, checksums, storage_class, parts";

fn row_to_object(r: &tokio_postgres::Row) -> Result<R2Object> {
    Ok(R2Object {
        key: r.get(0),
        version: r.get(1),
        size: r.get(2),
        etag: r.get(3),
        uploaded_ms: r.get(4),
        http_fields: r.get(5),
        custom_fields: r.get(6),
        checksums: r.get(7),
        storage_class: r.get(8),
        parts: serde_json::from_value(r.get(9))?,
    })
}

async fn garbage<C: tokio_postgres::GenericClient>(client: &C, parts: &[R2Part]) -> Result<()> {
    for p in parts {
        client
            .execute("INSERT INTO blob_garbage (path) VALUES ($1) ON CONFLICT DO NOTHING", &[&p.path])
            .await?;
    }
    Ok(())
}

async fn upsert_object<C: tokio_postgres::GenericClient>(
    client: &C,
    bucket: &str,
    o: &R2Object,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO r2_objects (bucket_id, key, version, size, etag, uploaded_ms, http_fields,
                                     custom_fields, checksums, storage_class, parts)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (bucket_id, key) DO UPDATE SET
               version = excluded.version, size = excluded.size, etag = excluded.etag,
               uploaded_ms = excluded.uploaded_ms, http_fields = excluded.http_fields,
               custom_fields = excluded.custom_fields, checksums = excluded.checksums,
               storage_class = excluded.storage_class, parts = excluded.parts",
            &[
                &bucket,
                &o.key,
                &o.version,
                &o.size,
                &o.etag,
                &o.uploaded_ms,
                &o.http_fields,
                &o.custom_fields,
                &o.checksums,
                &o.storage_class,
                &serde_json::to_value(&o.parts)?,
            ],
        )
        .await?;
    Ok(())
}

impl Db {
    pub async fn create_r2_bucket(&self, account: &AccountId, name: &str) -> Result<Option<String>> {
        let id = HexId::random();
        let r = self
            .client()
            .await?
            .execute(
                "INSERT INTO r2_buckets (id, account_id, name) VALUES ($1, $2, $3)",
                &[&id.as_str(), &account.as_str(), &name],
            )
            .await;
        match r {
            Ok(_) => Ok(Some(id.to_string())),
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// `(name, created)` for every bucket of the account.
    pub async fn r2_buckets(&self, account: &AccountId) -> Result<Vec<(String, std::time::SystemTime)>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT name, created_at FROM r2_buckets WHERE account_id = $1 ORDER BY name",
                &[&account.as_str()],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub async fn r2_bucket(
        &self,
        account: &AccountId,
        name: &str,
    ) -> Result<Option<(String, std::time::SystemTime)>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT id, created_at FROM r2_buckets WHERE account_id = $1 AND name = $2",
                &[&account.as_str(), &name],
            )
            .await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    pub async fn delete_r2_bucket(&self, account: &AccountId, name: &str) -> Result<BucketDelete> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let Some(row) = tx
            .query_opt(
                "SELECT id FROM r2_buckets WHERE account_id = $1 AND name = $2 FOR UPDATE",
                &[&account.as_str(), &name],
            )
            .await?
        else {
            return Ok(BucketDelete::NotFound);
        };
        let id: String = row.get(0);
        let busy = tx
            .query_opt(
                "SELECT 1 FROM r2_objects WHERE bucket_id = $1
                 UNION ALL SELECT 1 FROM r2_uploads WHERE bucket_id = $1 LIMIT 1",
                &[&id],
            )
            .await?
            .is_some();
        if busy {
            return Ok(BucketDelete::NotEmpty);
        }
        tx.execute("DELETE FROM r2_buckets WHERE id = $1", &[&id]).await?;
        tx.commit().await?;
        Ok(BucketDelete::Deleted)
    }

    pub async fn r2_head(&self, bucket: &str, key: &str) -> Result<Option<R2Object>> {
        let row = self
            .client()
            .await?
            .query_opt(
                &format!("SELECT {OBJECT_COLUMNS} FROM r2_objects WHERE bucket_id = $1 AND key = $2"),
                &[&bucket, &key],
            )
            .await?;
        row.as_ref().map(row_to_object).transpose()
    }

    /// Stores `object` if `allowed(current)` holds, atomically. Blobs that end up
    /// unreferenced (the old object's, or the new one's on a failed condition)
    /// are queued for deletion.
    /// Stores an object, if `allowed` holds for the current one, and in the
    /// same transaction enqueues the event notifications `event` triggers.
    pub async fn r2_put(
        &self,
        bucket: &str,
        object: &R2Object,
        allowed: impl FnOnce(Option<&R2Object>) -> bool,
        event: crate::r2notify::ObjectEvent<'_>,
    ) -> Result<PutOutcome> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        // Lock the row, or the key's gap when absent, against concurrent puts.
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended($1 || '/' || $2, 0))",
            &[&bucket, &object.key],
        )
        .await?;
        let current = tx
            .query_opt(
                &format!(
                    "SELECT {OBJECT_COLUMNS} FROM r2_objects WHERE bucket_id = $1 AND key = $2 FOR UPDATE"
                ),
                &[&bucket, &object.key],
            )
            .await?
            .as_ref()
            .map(row_to_object)
            .transpose()?;
        if !allowed(current.as_ref()) {
            garbage(&*tx, &object.parts).await?;
            tx.commit().await?;
            return Ok(PutOutcome::PreconditionFailed);
        }
        upsert_object(&*tx, bucket, object).await?;
        if let Some(old) = &current {
            garbage(&*tx, &old.parts).await?;
        }
        crate::r2notify::notify_in(&*tx, bucket, &object.key, Some((object.size, &object.etag)), event)
            .await?;
        tx.commit().await?;
        Ok(PutOutcome::Stored)
    }

    /// Deletes objects; returns the keys that existed.
    pub async fn r2_delete(&self, bucket: &str, keys: &[String]) -> Result<Vec<String>> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let rows = tx
            .query(
                "DELETE FROM r2_objects WHERE bucket_id = $1 AND key = ANY($2) RETURNING parts, key",
                &[&bucket, &keys],
            )
            .await?;
        let mut deleted = Vec::new();
        for r in rows {
            let parts: Vec<R2Part> = serde_json::from_value(r.get(0))?;
            garbage(&*tx, &parts).await?;
            let key: String = r.get(1);
            let event = crate::r2notify::ObjectEvent { action: "DeleteObject", copy_source: None };
            crate::r2notify::notify_in(&*tx, bucket, &key, None, event).await?;
            deleted.push(key);
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// Objects with `prefix`, after `after` (exclusive), in byte order.
    pub async fn r2_list(
        &self,
        bucket: &str,
        prefix: &str,
        after: &str,
        limit: i64,
    ) -> Result<Vec<R2Object>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT {OBJECT_COLUMNS} FROM r2_objects
                     WHERE bucket_id = $1 AND left(key, char_length($2)) = $2 AND key > $3
                     ORDER BY key LIMIT $4"
                ),
                &[&bucket, &prefix, &after, &limit],
            )
            .await?;
        rows.iter().map(row_to_object).collect()
    }

    // ---- multipart ----

    pub async fn r2_create_upload(&self, bucket: &str, upload: &R2Upload) -> Result<()> {
        self.client()
            .await?
            .execute(
                "INSERT INTO r2_uploads (id, bucket_id, key, http_fields, custom_fields, storage_class)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &upload.id,
                    &bucket,
                    &upload.key,
                    &upload.http_fields,
                    &upload.custom_fields,
                    &upload.storage_class,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn r2_upload(&self, bucket: &str, id: &str) -> Result<Option<R2Upload>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT key, http_fields, custom_fields, storage_class FROM r2_uploads
                 WHERE id = $1 AND bucket_id = $2",
                &[&id, &bucket],
            )
            .await?;
        Ok(row.map(|r| R2Upload {
            id: id.to_owned(),
            key: r.get(0),
            http_fields: r.get(1),
            custom_fields: r.get(2),
            storage_class: r.get(3),
        }))
    }

    /// Records a part. Returns false if the upload no longer exists (the blob is
    /// then queued for deletion).
    pub async fn r2_put_part(&self, upload: &str, part: &R2UploadedPart) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let exists = tx.query_opt("SELECT 1 FROM r2_uploads WHERE id = $1 FOR UPDATE", &[&upload]).await?;
        let blob = R2Part { path: part.path.clone(), size: part.size };
        if exists.is_none() {
            garbage(&*tx, &[blob]).await?;
            tx.commit().await?;
            return Ok(false);
        }
        let old = tx
            .query_opt(
                "SELECT path, size FROM r2_upload_parts WHERE upload_id = $1 AND part = $2",
                &[&upload, &part.part],
            )
            .await?;
        tx.execute(
            "INSERT INTO r2_upload_parts (upload_id, part, etag, size, path) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (upload_id, part) DO UPDATE SET etag = excluded.etag, size = excluded.size, path = excluded.path",
            &[&upload, &part.part, &part.etag, &part.size, &part.path],
        )
        .await?;
        if let Some(old) = old {
            garbage(&*tx, &[R2Part { path: old.get(0), size: old.get(1) }]).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn r2_upload_parts(&self, upload: &str) -> Result<Vec<R2UploadedPart>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT part, etag, size, path FROM r2_upload_parts WHERE upload_id = $1 ORDER BY part",
                &[&upload],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| R2UploadedPart { part: r.get(0), etag: r.get(1), size: r.get(2), path: r.get(3) })
            .collect())
    }

    /// Replaces the object with the completed upload and removes the upload.
    /// Parts not used by `object` are queued for deletion.
    pub async fn r2_complete_upload(&self, bucket: &str, upload: &str, object: &R2Object) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        if tx.query_opt("SELECT 1 FROM r2_uploads WHERE id = $1 FOR UPDATE", &[&upload]).await?.is_none() {
            return Ok(false);
        }
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended($1 || '/' || $2, 0))",
            &[&bucket, &object.key],
        )
        .await?;
        let old = tx
            .query_opt(
                "SELECT parts FROM r2_objects WHERE bucket_id = $1 AND key = $2 FOR UPDATE",
                &[&bucket, &object.key],
            )
            .await?;
        upsert_object(&*tx, bucket, object).await?;
        if let Some(old) = old {
            let parts: Vec<R2Part> = serde_json::from_value(old.get(0))?;
            garbage(&*tx, &parts).await?;
        }
        let used: Vec<&str> = object.parts.iter().map(|p| p.path.as_str()).collect();
        let unused = tx
            .query(
                "SELECT path, size FROM r2_upload_parts WHERE upload_id = $1 AND NOT (path = ANY($2))",
                &[&upload, &used],
            )
            .await?;
        let unused: Vec<R2Part> = unused.iter().map(|r| R2Part { path: r.get(0), size: r.get(1) }).collect();
        garbage(&*tx, &unused).await?;
        tx.execute("DELETE FROM r2_uploads WHERE id = $1", &[&upload]).await?;
        let event = crate::r2notify::ObjectEvent { action: "CompleteMultipartUpload", copy_source: None };
        crate::r2notify::notify_in(&*tx, bucket, &object.key, Some((object.size, &object.etag)), event)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn r2_abort_upload(&self, upload: &str) -> Result<()> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let parts =
            tx.query("SELECT path, size FROM r2_upload_parts WHERE upload_id = $1", &[&upload]).await?;
        let parts: Vec<R2Part> = parts.iter().map(|r| R2Part { path: r.get(0), size: r.get(1) }).collect();
        garbage(&*tx, &parts).await?;
        tx.execute("DELETE FROM r2_uploads WHERE id = $1", &[&upload]).await?;
        tx.commit().await?;
        Ok(())
    }

    // ---- garbage ----

    /// Blob paths queued at least `grace_secs` ago.
    pub async fn garbage_due(&self, grace_secs: i64, limit: i64) -> Result<Vec<String>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT path FROM blob_garbage WHERE created_at < now() - make_interval(secs => $1)
                 ORDER BY created_at LIMIT $2",
                &[&(grace_secs as f64), &limit],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    pub async fn garbage_done(&self, paths: &[String]) -> Result<()> {
        self.client().await?.execute("DELETE FROM blob_garbage WHERE path = ANY($1)", &[&paths]).await?;
        Ok(())
    }
}
