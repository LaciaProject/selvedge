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

/// What an object change needs to find its rules and describe itself.
#[derive(Debug, Clone)]
pub struct Targets {
    pub account_id: String,
    pub bucket_name: String,
    pub queues: Vec<QueueRules>,
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

    /// Everything an object change in the bucket needs to notify; `None`
    /// when the bucket has no rules.
    pub async fn r2_notification_targets(&self, bucket: &str) -> Result<Option<Targets>> {
        let client = self.client().await?;
        let rows = client.query(&format!("{RULE_QUERY} ORDER BY q.id, r.id"), &[&bucket]).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let b = client.query_one("SELECT account_id, name FROM r2_buckets WHERE id = $1", &[&bucket]).await?;
        Ok(Some(Targets { account_id: b.get(0), bucket_name: b.get(1), queues: group(&rows) }))
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
