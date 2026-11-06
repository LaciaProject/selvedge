//! Cron Triggers: storage and claiming of due firings.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use selvedge_core::AccountId;

use crate::Db;

/// Parses a Cloudflare cron expression (five fields, UTC; `L`, `W` and `#`).
/// Cloudflare writes "the last Friday" as `5L`; croner spells it `5#L`.
pub fn parse(cron: &str) -> Result<croner::Cron> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    anyhow::ensure!(fields.len() == 5, "invalid cron expression `{cron}`: expected five fields");
    let mut fields: Vec<String> = fields.iter().map(|f| (*f).to_owned()).collect();
    let dow = &fields[4];
    if let Some(day) = dow.strip_suffix('L').filter(|d| !d.is_empty() && !d.ends_with('#')) {
        fields[4] = format!("{day}#L");
    }
    fields.join(" ").parse::<croner::Cron>().with_context(|| format!("invalid cron expression `{cron}`"))
}

/// The first firing strictly after `after`.
pub fn next_after(cron: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>> {
    parse(cron)?.find_next_occurrence(&after, false).with_context(|| format!("`{cron}` never fires"))
}

/// A firing claimed for dispatch.
#[derive(Debug, Clone)]
pub struct Firing {
    pub account: AccountId,
    pub script: String,
    pub cron: String,
    pub scheduled_ms: i64,
}

impl Db {
    /// Replaces a script's triggers. Errors on an invalid expression.
    pub async fn set_schedules(&self, account: &AccountId, script: &str, crons: &[String]) -> Result<bool> {
        let now = Utc::now();
        let mut next = Vec::new();
        for c in crons {
            next.push((c.clone(), next_after(c, now)?));
        }
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let exists = tx
            .query_opt(
                "SELECT 1 FROM scripts WHERE account_id = $1 AND name = $2",
                &[&account.as_str(), &script],
            )
            .await?;
        if exists.is_none() {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM script_schedules WHERE account_id = $1 AND script_name = $2 AND NOT (cron = ANY($3))",
            &[&account.as_str(), &script, &crons],
        )
        .await?;
        for (cron, at) in next {
            let at: std::time::SystemTime = at.into();
            tx.execute(
                "INSERT INTO script_schedules (account_id, script_name, cron, next_at) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (account_id, script_name, cron) DO NOTHING",
                &[&account.as_str(), &script, &cron, &at],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn schedules(&self, account: &AccountId, script: &str) -> Result<Vec<(String, String)>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT cron, created_at FROM script_schedules WHERE account_id = $1 AND script_name = $2 ORDER BY cron",
                &[&account.as_str(), &script],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let t: std::time::SystemTime = r.get(1);
                (r.get(0), DateTime::<Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Micros, true))
            })
            .collect())
    }

    /// Claims every due firing and advances its trigger, in one transaction.
    /// Concurrent dispatchers skip each other's rows, so each firing is claimed
    /// once. A trigger that fell far behind fires once, not once per missed slot.
    pub async fn claim_due_firings(&self, limit: i64) -> Result<Vec<Firing>> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let rows = tx
            .query(
                "SELECT account_id, script_name, cron, next_at FROM script_schedules
                 WHERE next_at <= now() ORDER BY next_at LIMIT $1 FOR UPDATE SKIP LOCKED",
                &[&limit],
            )
            .await?;
        let now = Utc::now();
        let mut out = Vec::new();
        for r in rows {
            let (account, script, cron): (String, String, String) = (r.get(0), r.get(1), r.get(2));
            let due: DateTime<Utc> = DateTime::from(r.get::<_, std::time::SystemTime>(3));
            let next: std::time::SystemTime = match next_after(&cron, now) {
                Ok(n) => n.into(),
                Err(e) => {
                    tracing::warn!(%account, %script, %cron, error = %e, "dropping an unusable trigger");
                    tx.execute(
                        "DELETE FROM script_schedules WHERE account_id = $1 AND script_name = $2 AND cron = $3",
                        &[&account, &script, &cron],
                    )
                    .await?;
                    continue;
                }
            };
            tx.execute(
                "UPDATE script_schedules SET next_at = $4 WHERE account_id = $1 AND script_name = $2 AND cron = $3",
                &[&account, &script, &cron, &next],
            )
            .await?;
            out.push(Firing {
                account: account.parse()?,
                script,
                cron,
                scheduled_ms: due.timestamp_millis(),
            });
        }
        tx.commit().await?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn expressions() {
        let t = Utc.with_ymd_and_hms(2026, 11, 6, 10, 2, 30).unwrap();
        assert_eq!(
            next_after("*/5 * * * *", t).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 6, 10, 5, 0).unwrap()
        );
        assert_eq!(next_after("0 0 * * *", t).unwrap(), Utc.with_ymd_and_hms(2026, 11, 7, 0, 0, 0).unwrap());
        // The last Friday of the month.
        assert_eq!(
            next_after("0 12 * * 5L", t).unwrap(),
            Utc.with_ymd_and_hms(2026, 11, 27, 12, 0, 0).unwrap()
        );
        assert!(parse("not a cron").is_err());
    }
}
