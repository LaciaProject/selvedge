//! R2 event notifications: rules that send object changes to queues.

use anyhow::Result;
use selvedge_core::HexId;

use crate::Db;

/// One rule: objects whose key has `prefix` and `suffix`, changed by one of
/// `actions` (`PutObject`, `CopyObject`, `CompleteMultipartUpload`,
/// `DeleteObject`, `LifecycleDeletion`).
#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub prefix: String,
    pub suffix: String,
    pub actions: Vec<String>,
    pub description: Option<String>,
    pub created_at: std::time::SystemTime,
}

impl Rule {
    pub fn matches(&self, action: &str, key: &str) -> bool {
        self.actions.iter().any(|a| a == action)
            && key.starts_with(&self.prefix)
            && key.ends_with(&self.suffix)
    }
}

/// A queue a bucket's rules send to.
#[derive(Debug, Clone)]
pub struct QueueRules {
    pub queue_id: String,
    pub queue_name: String,
    pub rules: Vec<Rule>,
}

/// An object change, as event notifications describe it.
#[derive(Debug, Clone, Copy)]
pub struct ObjectEvent<'a> {
    /// `PutObject`, `CopyObject`, `CompleteMultipartUpload` or `DeleteObject`.
    pub action: &'a str,
    /// For copies: the source bucket (by name) and key.
    pub copy_source: Option<(&'a str, &'a str)>,
}

/// Enqueues the notifications an object change triggers, in the change's own
/// transaction: a message to each queue with a matching rule. `object` is
/// `(size, etag)` of the new object, `None` for a deletion.
pub(crate) async fn notify_in<C: tokio_postgres::GenericClient>(
    tx: &C,
    bucket: &str,
    key: &str,
    object: Option<(i64, &str)>,
    event: ObjectEvent<'_>,
) -> Result<()> {
    let (size, etag) = object.map_or((None, None), |(s, e)| (Some(s), Some(e)));
    let (src_bucket, src_key) = event.copy_source.map_or((None, None), |(b, k)| (Some(b), Some(k)));
    tx.execute(
        "INSERT INTO queue_messages (id, queue_id, body, content_type, visible_at)
         SELECT replace(gen_random_uuid()::text, '-', ''), q.queue_id,
                convert_to((jsonb_build_object(
                    'account', b.account_id,
                    'action', $2::text,
                    'bucket', b.name,
                    'object', CASE WHEN $4::bigint IS NULL THEN jsonb_build_object('key', $3::text)
                                   ELSE jsonb_build_object('key', $3::text, 'size', $4::bigint, 'eTag', $5::text) END,
                    'eventTime', to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'))
                  || CASE WHEN $6::text IS NULL THEN '{}'::jsonb
                          ELSE jsonb_build_object('copySource', jsonb_build_object('bucket', $6::text, 'object', $7::text)) END
                )::text, 'UTF8'),
                'json',
                now() + make_interval(secs => qq.delivery_delay_secs)
         FROM (SELECT DISTINCT r.queue_id FROM r2_notification_rules r
               WHERE r.bucket_id = $1 AND $2::text = ANY(r.actions)
                 AND starts_with($3::text, r.prefix) AND right($3::text, length(r.suffix)) = r.suffix) q
         JOIN queues qq ON qq.id = q.queue_id
         JOIN r2_buckets b ON b.id = $1",
        &[&bucket, &event.action, &key, &size, &etag, &src_bucket, &src_key],
    )
    .await?;
    Ok(())
}

fn rule(r: &tokio_postgres::Row) -> Rule {
    Rule {
        id: r.get(2),
        prefix: r.get(3),
        suffix: r.get(4),
        actions: r.get(5),
        description: r.get(6),
        created_at: r.get(7),
    }
}

fn group(rows: &[tokio_postgres::Row]) -> Vec<QueueRules> {
    let mut out: Vec<QueueRules> = Vec::new();
    for r in rows {
        let queue_id: String = r.get(0);
        match out.last_mut() {
            Some(q) if q.queue_id == queue_id => q.rules.push(rule(r)),
            _ => out.push(QueueRules { queue_id, queue_name: r.get(1), rules: vec![rule(r)] }),
        }
    }
    out
}

const RULE_QUERY: &str =
    "SELECT q.id, q.name, r.id, r.prefix, r.suffix, r.actions, r.description, r.created_at
     FROM r2_notification_rules r JOIN queues q ON q.id = r.queue_id
     WHERE r.bucket_id = $1";

impl Db {
    /// Adds rules sending to a queue; returns their ids.
    pub async fn add_r2_notification_rules(
        &self,
        bucket: &str,
        queue: &str,
        rules: &[(String, String, Vec<String>, Option<String>)],
    ) -> Result<Vec<String>> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let mut ids = Vec::new();
        for (prefix, suffix, actions, description) in rules {
            let id = HexId::random().to_string();
            tx.execute(
                "INSERT INTO r2_notification_rules (id, bucket_id, queue_id, prefix, suffix, actions, description)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)",
                &[&id, &bucket, &queue, prefix, suffix, actions, description],
            )
            .await?;
            ids.push(id);
        }
        tx.commit().await?;
        Ok(ids)
    }

    /// The bucket's rules, by queue (`queue`: only that one).
    pub async fn r2_notification_rules(&self, bucket: &str, queue: Option<&str>) -> Result<Vec<QueueRules>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!("{RULE_QUERY} AND ($2::text IS NULL OR r.queue_id = $2) ORDER BY q.name, q.id, r.created_at, r.id"),
                &[&bucket, &queue],
            )
            .await?;
        Ok(group(&rows))
    }

    /// Deletes the queue's rules on the bucket (`rule_ids`: only those).
    /// Returns how many went.
    pub async fn delete_r2_notification_rules(
        &self,
        bucket: &str,
        queue: &str,
        rule_ids: Option<&[String]>,
    ) -> Result<u64> {
        Ok(self
            .client()
            .await?
            .execute(
                "DELETE FROM r2_notification_rules WHERE bucket_id = $1 AND queue_id = $2
                   AND ($3::text[] IS NULL OR id = ANY($3))",
                &[&bucket, &queue, &rule_ids],
            )
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_match_action_prefix_and_suffix() {
        let r = Rule {
            id: "r".into(),
            prefix: "img/".into(),
            suffix: ".png".into(),
            actions: vec!["PutObject".into(), "CopyObject".into()],
            description: None,
            created_at: std::time::SystemTime::UNIX_EPOCH,
        };
        assert!(r.matches("PutObject", "img/a.png"));
        assert!(r.matches("CopyObject", "img/b/c.png"));
        assert!(!r.matches("DeleteObject", "img/a.png"));
        assert!(!r.matches("PutObject", "doc/a.png"));
        assert!(!r.matches("PutObject", "img/a.jpg"));
        let all = Rule { prefix: String::new(), suffix: String::new(), ..r };
        assert!(all.matches("PutObject", "anything"));
    }
}
