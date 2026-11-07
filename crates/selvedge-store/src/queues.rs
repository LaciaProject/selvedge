//! Queues: producers insert rows; dispatchers lease batches. A delivered
//! message stays invisible for the lease; acking deletes it, retrying makes
//! it visible again after the delay, and running out of retries moves it to
//! the dead-letter queue (or drops it).

use std::time::{Duration, SystemTime};

use anyhow::Result;
use selvedge_core::{AccountId, HexId};

use crate::Db;

#[derive(Debug, Clone, serde::Serialize)]
pub struct QueueInfo {
    pub id: String,
    pub name: String,
    pub created_on: String,
    pub delivery_paused: bool,
    pub delivery_delay_secs: i32,
    pub retention_secs: i32,
}

const QUEUE_COLUMNS: &str = "id, name, created_at, delivery_paused, delivery_delay_secs, retention_secs";

fn queue_info(r: &tokio_postgres::Row) -> QueueInfo {
    QueueInfo {
        id: r.get(0),
        name: r.get(1),
        created_on: crate::repo::rfc3339(r.get(2)),
        delivery_paused: r.get(3),
        delivery_delay_secs: r.get(4),
        retention_secs: r.get(5),
    }
}

/// Changes to a queue's settings; `None` leaves one as it is.
#[derive(Debug, Default)]
pub struct QueueUpdate {
    pub name: Option<String>,
    pub delivery_paused: Option<bool>,
    pub delivery_delay_secs: Option<i32>,
    pub retention_secs: Option<i32>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ConsumerSettings {
    /// `worker` (delivered to `script`) or `http_pull` (pulled over the API;
    /// `script` is empty).
    #[serde(default = "worker_kind")]
    pub kind: String,
    pub script: String,
    pub batch_size: i32,
    pub max_wait_ms: i32,
    pub max_retries: i32,
    pub retry_delay_secs: i32,
    pub dead_letter_queue: Option<String>,
    /// HTTP pull: how long a pulled message stays leased.
    #[serde(default = "default_visibility_ms")]
    pub visibility_timeout_ms: i32,
}

fn worker_kind() -> String {
    "worker".into()
}

fn default_visibility_ms() -> i32 {
    30_000
}

impl ConsumerSettings {
    pub fn is_http_pull(&self) -> bool {
        self.kind == "http_pull"
    }
}

/// A pulled message's lease: the message and the attempt it was leased for.
/// A lease from an earlier attempt (the message timed out and was pulled
/// again) no longer settles it.
pub fn lease_id(m: &Message) -> String {
    format!("{}.{}", m.id, m.attempts)
}

fn parse_lease(lease: &str) -> Option<(&str, i32)> {
    let (id, attempts) = lease.rsplit_once('.')?;
    Some((id, attempts.parse().ok()?))
}

#[derive(Debug, Clone)]
pub struct Consumer {
    pub queue_id: String,
    pub queue_name: String,
    pub account: AccountId,
    pub settings: ConsumerSettings,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: String,
    pub body: Vec<u8>,
    pub content_type: String,
    pub attempts: i32,
    pub timestamp_ms: i64,
}

pub struct NewMessage {
    pub body: Vec<u8>,
    pub content_type: String,
    /// `None`: the queue's delivery delay.
    pub delay_secs: Option<u32>,
}

/// What happens to one delivered message.
pub enum Outcome {
    Ack,
    Retry { delay_secs: u32 },
}

fn ms(t: SystemTime) -> i64 {
    t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or_default()
}

impl Db {
    pub async fn create_queue(&self, account: &AccountId, name: &str) -> Result<Option<QueueInfo>> {
        let id = HexId::random().to_string();
        let r = self
            .client()
            .await?
            .query_one(
                &format!(
                    "INSERT INTO queues (id, account_id, name) VALUES ($1, $2, $3) RETURNING {QUEUE_COLUMNS}"
                ),
                &[&id, &account.as_str(), &name],
            )
            .await;
        match r {
            Ok(row) => Ok(Some(queue_info(&row))),
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn queues(&self, account: &AccountId) -> Result<Vec<QueueInfo>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!("SELECT {QUEUE_COLUMNS} FROM queues WHERE account_id = $1 ORDER BY name"),
                &[&account.as_str()],
            )
            .await?;
        Ok(rows.iter().map(queue_info).collect())
    }

    /// A queue by id or name.
    pub async fn queue(&self, account: &AccountId, id_or_name: &str) -> Result<Option<QueueInfo>> {
        let row = self
            .client()
            .await?
            .query_opt(
                &format!(
                    "SELECT {QUEUE_COLUMNS} FROM queues WHERE account_id = $1 AND (id = $2 OR name = $2)"
                ),
                &[&account.as_str(), &id_or_name],
            )
            .await?;
        Ok(row.as_ref().map(queue_info))
    }

    /// Updates a queue's settings; `None` if the queue is gone, `Err` on a
    /// name another queue of the account has.
    pub async fn update_queue(
        &self,
        account: &AccountId,
        id: &str,
        u: &QueueUpdate,
    ) -> Result<Option<QueueInfo>> {
        let row = self
            .client()
            .await?
            .query_opt(
                &format!(
                    "UPDATE queues SET name = COALESCE($3, name),
                       delivery_paused = COALESCE($4, delivery_paused),
                       delivery_delay_secs = COALESCE($5, delivery_delay_secs),
                       retention_secs = COALESCE($6, retention_secs)
                     WHERE account_id = $1 AND id = $2 RETURNING {QUEUE_COLUMNS}"
                ),
                &[
                    &account.as_str(),
                    &id,
                    &u.name,
                    &u.delivery_paused,
                    &u.delivery_delay_secs,
                    &u.retention_secs,
                ],
            )
            .await?;
        Ok(row.as_ref().map(queue_info))
    }

    /// Deletes every message of a queue; returns how many.
    pub async fn purge_queue(&self, id: &str) -> Result<u64> {
        Ok(self.client().await?.execute("DELETE FROM queue_messages WHERE queue_id = $1", &[&id]).await?)
    }

    /// Drops messages older than their queue's retention period.
    pub async fn purge_retained_messages(&self) -> Result<u64> {
        Ok(self
            .client()
            .await?
            .execute(
                "DELETE FROM queue_messages m USING queues q
                 WHERE m.queue_id = q.id AND m.created_at < now() - make_interval(secs => q.retention_secs)",
                &[],
            )
            .await?)
    }

    pub async fn delete_queue(&self, account: &AccountId, id: &str) -> Result<bool> {
        let n = self
            .client()
            .await?
            .execute("DELETE FROM queues WHERE account_id = $1 AND id = $2", &[&account.as_str(), &id])
            .await?;
        Ok(n == 1)
    }

    pub async fn set_consumer(
        &self,
        account: &AccountId,
        queue_id: &str,
        s: &ConsumerSettings,
    ) -> Result<()> {
        self.client()
            .await?
            .execute(
                "INSERT INTO queue_consumers (queue_id, account_id, script_name, batch_size, max_wait_ms,
                                              max_retries, retry_delay_secs, dead_letter_queue, kind,
                                              visibility_timeout_ms)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT (queue_id) DO UPDATE SET script_name = excluded.script_name,
                   batch_size = excluded.batch_size, max_wait_ms = excluded.max_wait_ms,
                   max_retries = excluded.max_retries, retry_delay_secs = excluded.retry_delay_secs,
                   dead_letter_queue = excluded.dead_letter_queue, kind = excluded.kind,
                   visibility_timeout_ms = excluded.visibility_timeout_ms",
                &[
                    &queue_id,
                    &account.as_str(),
                    &(!s.is_http_pull()).then_some(s.script.as_str()),
                    &s.batch_size,
                    &s.max_wait_ms,
                    &s.max_retries,
                    &s.retry_delay_secs,
                    &s.dead_letter_queue,
                    &s.kind,
                    &s.visibility_timeout_ms,
                ],
            )
            .await?;
        Ok(())
    }

    pub async fn consumer(&self, queue_id: &str) -> Result<Option<ConsumerSettings>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT script_name, batch_size, max_wait_ms, max_retries, retry_delay_secs, dead_letter_queue,
                        kind, visibility_timeout_ms
                 FROM queue_consumers WHERE queue_id = $1",
                &[&queue_id],
            )
            .await?;
        Ok(row.map(|r| ConsumerSettings {
            script: r.get::<_, Option<String>>(0).unwrap_or_default(),
            batch_size: r.get(1),
            max_wait_ms: r.get(2),
            max_retries: r.get(3),
            retry_delay_secs: r.get(4),
            dead_letter_queue: r.get(5),
            kind: r.get(6),
            visibility_timeout_ms: r.get(7),
        }))
    }

    pub async fn delete_consumer(&self, queue_id: &str) -> Result<bool> {
        Ok(self
            .client()
            .await?
            .execute("DELETE FROM queue_consumers WHERE queue_id = $1", &[&queue_id])
            .await?
            == 1)
    }

    pub async fn enqueue(&self, queue_id: &str, messages: &[NewMessage]) -> Result<()> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        for m in messages {
            let delay = m.delay_secs.map(f64::from);
            tx.execute(
                "INSERT INTO queue_messages (id, queue_id, body, content_type, visible_at)
                 SELECT $1, q.id, $3, $4, now() + make_interval(secs => COALESCE($5::float8, q.delivery_delay_secs))
                 FROM queues q WHERE q.id = $2",
                &[&selvedge_core::new_version_id(), &queue_id, &m.body, &m.content_type, &delay],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// `(messages, bytes)` waiting in a queue.
    pub async fn backlog(&self, queue_id: &str) -> Result<(i64, i64)> {
        let r = self
            .client()
            .await?
            .query_one(
                "SELECT count(*), coalesce(sum(length(body)), 0)::bigint FROM queue_messages WHERE queue_id = $1",
                &[&queue_id],
            )
            .await?;
        Ok((r.get(0), r.get(1)))
    }

    /// Queues that have a Worker consumer, are not paused, and have a batch
    /// ready: `batch_size` visible messages, or one that has waited
    /// `max_wait_ms` (`max_batch_timeout`).
    pub async fn ready_consumers(&self) -> Result<Vec<Consumer>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT q.id, q.name, c.account_id, c.script_name, c.batch_size, c.max_wait_ms, c.max_retries,
                        c.retry_delay_secs, c.dead_letter_queue
                 FROM queue_consumers c JOIN queues q ON q.id = c.queue_id
                 WHERE c.kind = 'worker' AND NOT q.delivery_paused
                   AND (EXISTS (SELECT 1 FROM queue_messages m WHERE m.queue_id = q.id
                                  AND m.visible_at <= now() - make_interval(secs => c.max_wait_ms / 1000.0))
                        OR (SELECT count(*) FROM (SELECT 1 FROM queue_messages m
                                                  WHERE m.queue_id = q.id AND m.visible_at <= now()
                                                  LIMIT c.batch_size) b) >= c.batch_size)",
                &[],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(Consumer {
                    queue_id: r.get(0),
                    queue_name: r.get(1),
                    account: r.get::<_, String>(2).parse()?,
                    settings: ConsumerSettings {
                        script: r.get(3),
                        batch_size: r.get(4),
                        max_wait_ms: r.get(5),
                        max_retries: r.get(6),
                        retry_delay_secs: r.get(7),
                        dead_letter_queue: r.get(8),
                        kind: "worker".into(),
                        visibility_timeout_ms: default_visibility_ms(),
                    },
                })
            })
            .collect()
    }

    /// Leases up to `limit` visible messages for `lease`, counting an attempt.
    pub async fn lease_batch(&self, queue_id: &str, limit: i64, lease: Duration) -> Result<Vec<Message>> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let rows = tx
            .query(
                "UPDATE queue_messages SET attempts = attempts + 1, visible_at = now() + make_interval(secs => $3)
                 WHERE id IN (SELECT id FROM queue_messages WHERE queue_id = $1 AND visible_at <= now()
                              ORDER BY visible_at, created_at LIMIT $2 FOR UPDATE SKIP LOCKED)
                 RETURNING id, body, content_type, attempts, created_at",
                &[&queue_id, &limit, &lease.as_secs_f64()],
            )
            .await?;
        tx.commit().await?;
        let mut out: Vec<Message> = rows
            .iter()
            .map(|r| Message {
                id: r.get(0),
                body: r.get(1),
                content_type: r.get(2),
                attempts: r.get(3),
                timestamp_ms: ms(r.get(4)),
            })
            .collect();
        out.sort_by_key(|m| m.timestamp_ms);
        Ok(out)
    }

    /// Applies a batch's outcome. Messages retried past `max_retries` go to the
    /// dead-letter queue when there is one, else they are dropped.
    pub async fn settle(
        &self,
        account: &AccountId,
        consumer: &ConsumerSettings,
        results: &[(Message, Outcome)],
    ) -> Result<()> {
        let dlq = match &consumer.dead_letter_queue {
            Some(name) => self.queue(account, name).await?.map(|q| q.id),
            None => None,
        };
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        for (m, outcome) in results {
            match outcome {
                Outcome::Ack => {
                    tx.execute("DELETE FROM queue_messages WHERE id = $1", &[&m.id]).await?;
                }
                Outcome::Retry { .. } if m.attempts > consumer.max_retries => match &dlq {
                    Some(dlq) => {
                        tx.execute(
                            "UPDATE queue_messages SET queue_id = $2, attempts = 0, visible_at = now() WHERE id = $1",
                            &[&m.id, dlq],
                        )
                        .await?;
                    }
                    None => {
                        tx.execute("DELETE FROM queue_messages WHERE id = $1", &[&m.id]).await?;
                    }
                },
                Outcome::Retry { delay_secs } => {
                    tx.execute(
                        "UPDATE queue_messages SET visible_at = now() + make_interval(secs => $2) WHERE id = $1",
                        &[&m.id, &f64::from(*delay_secs)],
                    )
                    .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// HTTP pull: messages whose lease ran out after their last allowed
    /// attempt move to the dead-letter queue (or are dropped), as a retry
    /// past `max_retries` does.
    pub async fn expire_pulled(
        &self,
        account: &AccountId,
        queue_id: &str,
        c: &ConsumerSettings,
    ) -> Result<()> {
        let dlq = match &c.dead_letter_queue {
            Some(name) => self.queue(account, name).await?.map(|q| q.id),
            None => None,
        };
        let client = self.client().await?;
        let exhausted = "queue_id = $1 AND visible_at <= now() AND attempts > $2";
        match dlq {
            Some(dlq) => {
                client
                    .execute(
                        &format!("UPDATE queue_messages SET queue_id = $3, attempts = 0 WHERE {exhausted}"),
                        &[&queue_id, &c.max_retries, &dlq],
                    )
                    .await?
            }
            None => {
                client
                    .execute(
                        &format!("DELETE FROM queue_messages WHERE {exhausted}"),
                        &[&queue_id, &c.max_retries],
                    )
                    .await?
            }
        };
        Ok(())
    }

    /// HTTP pull: acks and retries by lease id. Returns how many of each
    /// settled; leases that no longer hold (past their visibility timeout, or
    /// from an earlier attempt) are skipped.
    pub async fn settle_pulled(
        &self,
        account: &AccountId,
        queue_id: &str,
        c: &ConsumerSettings,
        acks: &[String],
        retries: &[(String, u32)],
    ) -> Result<(u64, u64)> {
        let dlq = match &c.dead_letter_queue {
            Some(name) => self.queue(account, name).await?.map(|q| q.id),
            None => None,
        };
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let (mut acked, mut retried) = (0, 0);
        for lease in acks {
            let Some((id, attempts)) = parse_lease(lease) else { continue };
            acked += tx
                .execute(
                    "DELETE FROM queue_messages WHERE id = $1 AND queue_id = $2 AND attempts = $3 AND visible_at > now()",
                    &[&id, &queue_id, &attempts],
                )
                .await?;
        }
        for (lease, delay) in retries {
            let Some((id, attempts)) = parse_lease(lease) else { continue };
            let n = if attempts > c.max_retries {
                match &dlq {
                    Some(dlq) => {
                        tx.execute(
                            "UPDATE queue_messages SET queue_id = $4, attempts = 0, visible_at = now()
                             WHERE id = $1 AND queue_id = $2 AND attempts = $3 AND visible_at > now()",
                            &[&id, &queue_id, &attempts, dlq],
                        )
                        .await?
                    }
                    None => {
                        tx.execute(
                            "DELETE FROM queue_messages WHERE id = $1 AND queue_id = $2 AND attempts = $3 AND visible_at > now()",
                            &[&id, &queue_id, &attempts],
                        )
                        .await?
                    }
                }
            } else {
                tx.execute(
                    "UPDATE queue_messages SET visible_at = now() + make_interval(secs => $4)
                     WHERE id = $1 AND queue_id = $2 AND attempts = $3 AND visible_at > now()",
                    &[&id, &queue_id, &attempts, &f64::from(*delay)],
                )
                .await?
            };
            retried += n;
        }
        tx.commit().await?;
        Ok((acked, retried))
    }
}
