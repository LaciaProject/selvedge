//! `wrangler tail` sessions, and PostgreSQL `LISTEN`/`NOTIFY` to carry trace
//! events to whichever API replica holds a session's WebSocket.

use anyhow::Result;
use futures::StreamExt as _;
use selvedge_core::AccountId;
use serde_json::Value;
use tokio_postgres::{AsyncMessage, NoTls};

use crate::Db;

/// How long a tail session lives.
pub const TAIL_TTL_SECS: i64 = 4 * 3600;

/// The channel trace events are published on.
pub const TAIL_CHANNEL: &str = "selvedge_tail";

/// PostgreSQL's limit on a `NOTIFY` payload is 8000 bytes.
pub const MAX_NOTIFY_BYTES: usize = 7900;

#[derive(Debug, Clone)]
pub struct TailSession {
    pub id: String,
    pub account: AccountId,
    pub script: String,
    pub filters: Value,
    pub creator_ip: Option<String>,
    pub expires_at: String,
    /// Seconds until the session expires.
    pub expires_in: i64,
}

fn row_to_session(r: &tokio_postgres::Row) -> Result<TailSession> {
    Ok(TailSession {
        id: r.get(0),
        account: r.get::<_, String>(1).parse()?,
        script: r.get(2),
        filters: r.get(3),
        creator_ip: r.get(4),
        expires_at: r.get(5),
        expires_in: r.get(6),
    })
}

const SESSION_COLUMNS: &str = "id, account_id, script_name, filters, creator_ip,
    to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
    greatest(0, extract(epoch FROM expires_at - now()))::bigint";

impl Db {
    pub async fn create_tail(
        &self,
        account: &AccountId,
        script: &str,
        filters: &Value,
        token_sha256: &str,
        creator_ip: Option<&str>,
    ) -> Result<TailSession> {
        let client = self.client().await?;
        client.execute("DELETE FROM tail_sessions WHERE expires_at < now()", &[]).await?;
        let id = selvedge_core::new_version_id();
        let row = client
            .query_one(
                &format!(
                    "INSERT INTO tail_sessions (id, account_id, script_name, filters, token_sha256, creator_ip, expires_at)
                     VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))
                     RETURNING {SESSION_COLUMNS}"
                ),
                &[
                    &id,
                    &account.as_str(),
                    &script,
                    filters,
                    &token_sha256,
                    &creator_ip,
                    &(TAIL_TTL_SECS as f64),
                ],
            )
            .await?;
        row_to_session(&row)
    }

    pub async fn tails(&self, account: &AccountId, script: &str) -> Result<Vec<TailSession>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM tail_sessions
                     WHERE account_id = $1 AND script_name = $2 AND expires_at > now() ORDER BY created_at"
                ),
                &[&account.as_str(), &script],
            )
            .await?;
        rows.iter().map(row_to_session).collect()
    }

    pub async fn tail_by_token(&self, token_sha256: &str) -> Result<Option<TailSession>> {
        let row = self
            .client()
            .await?
            .query_opt(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM tail_sessions WHERE token_sha256 = $1 AND expires_at > now()"
                ),
                &[&token_sha256],
            )
            .await?;
        row.as_ref().map(row_to_session).transpose()
    }

    /// Deletes a session; `false` if there was none.
    pub async fn delete_tail(&self, account: &AccountId, script: &str, id: &str) -> Result<bool> {
        let n = self
            .client()
            .await?
            .execute(
                "DELETE FROM tail_sessions WHERE account_id = $1 AND script_name = $2 AND id = $3",
                &[&account.as_str(), &script, &id],
            )
            .await?;
        Ok(n > 0)
    }

    /// Every (account, script) with a live session.
    pub async fn tailed_scripts(&self) -> Result<Vec<(String, String)>> {
        let rows = self
            .client()
            .await?
            .query("SELECT DISTINCT account_id, script_name FROM tail_sessions WHERE expires_at > now()", &[])
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub async fn notify(&self, channel: &str, payload: &str) -> Result<()> {
        self.client().await?.execute("SELECT pg_notify($1, $2)", &[&channel, &payload]).await?;
        Ok(())
    }

    /// Payloads published on `channel`, from a dedicated connection that
    /// reconnects on failure. Notifications sent while it is reconnecting are
    /// lost.
    pub fn listen(&self, channel: &'static str) -> tokio::sync::mpsc::Receiver<String> {
        let (tx, rx) = tokio::sync::mpsc::channel(4096);
        let config = self.config.clone();
        tokio::spawn(async move {
            while !tx.is_closed() {
                match config.connect(NoTls).await {
                    Ok((client, mut conn)) => {
                        let tx = tx.clone();
                        let pump = tokio::spawn(async move {
                            let mut messages = futures::stream::poll_fn(move |cx| conn.poll_message(cx));
                            while let Some(m) = messages.next().await {
                                match m {
                                    Ok(AsyncMessage::Notification(n)) => {
                                        // A full queue drops events rather than stalling the connection.
                                        let _ = tx.try_send(n.payload().to_owned());
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!(error = %e, "LISTEN connection failed");
                                        break;
                                    }
                                }
                            }
                        });
                        if let Err(e) = client.batch_execute(&format!("LISTEN {channel}")).await {
                            tracing::warn!(error = %e, "LISTEN failed");
                        }
                        let _ = pump.await;
                        drop(client);
                    }
                    Err(e) => tracing::warn!(error = %e, "connecting for LISTEN failed"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
        rx
    }
}
