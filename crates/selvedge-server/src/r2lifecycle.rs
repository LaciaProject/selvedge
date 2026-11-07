//! R2 object lifecycle rules, as `wrangler r2 bucket lifecycle` manages them:
//! delete objects under a prefix after an age or on a date, and abort
//! multipart uploads left incomplete. The data service applies them.

use anyhow::Result;
use selvedge_store::Db;
use serde_json::{Value, json};

/// Cloudflare's rule for a bucket that has none configured.
pub fn default_rules() -> Value {
    json!([{
        "id": "Default Multipart Abort Rule",
        "enabled": true,
        "conditions": { "prefix": "" },
        "abortMultipartUploadsTransition": { "condition": { "maxAge": 604_800, "type": "Age" } },
    }])
}

/// When a rule's action applies: to objects older than an age, or to all
/// of them once a date has passed.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    Age(i64),
    Date(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub id: String,
    pub enabled: bool,
    pub prefix: String,
    pub delete: Option<Condition>,
    pub abort_uploads_after: Option<i64>,
}

fn condition(v: &Value, what: &str) -> Result<Condition, String> {
    let c = &v["condition"];
    match c["type"].as_str() {
        Some("Age") => match c["maxAge"].as_i64() {
            Some(age) if age >= 0 => Ok(Condition::Age(age)),
            _ => Err(format!("{what}: an `Age` condition needs `maxAge`, in seconds")),
        },
        Some("Date") => c["date"]
            .as_str()
            .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
            .map(|d| Condition::Date(d.timestamp_millis()))
            .ok_or_else(|| format!("{what}: a `Date` condition needs `date`, an RFC 3339 time")),
        _ => Err(format!("{what}: the condition `type` must be `Age` or `Date`")),
    }
}

/// Parses and checks the rules of a `PUT .../lifecycle` body.
pub fn parse_rules(rules: &Value) -> Result<Vec<Rule>, String> {
    let list = rules.as_array().ok_or("`rules` must be a list")?;
    if list.len() > 1000 {
        return Err("a bucket may have at most 1000 lifecycle rules".into());
    }
    let mut out: Vec<Rule> = Vec::new();
    for r in list {
        let id = r["id"].as_str().filter(|i| !i.is_empty()).ok_or("every rule needs an `id`")?.to_owned();
        if out.iter().any(|o| o.id == id) {
            return Err(format!("two rules have the id `{id}`"));
        }
        let delete = match r.get("deleteObjectsTransition") {
            Some(v) if !v.is_null() => Some(condition(v, &id)?),
            _ => None,
        };
        let abort_uploads_after = match r.get("abortMultipartUploadsTransition") {
            Some(v) if !v.is_null() => match condition(v, &id)? {
                Condition::Age(a) => Some(a),
                Condition::Date(_) => return Err(format!("{id}: aborting uploads takes an `Age` condition")),
            },
            _ => None,
        };
        out.push(Rule {
            id,
            enabled: r["enabled"].as_bool().unwrap_or(true),
            prefix: r["conditions"]["prefix"].as_str().unwrap_or("").to_owned(),
            delete,
            abort_uploads_after,
        });
    }
    Ok(out)
}

/// How many objects or uploads one pass handles per rule.
const BATCH: i64 = 1000;

/// One pass over every bucket's rules: deletes expired objects (sending
/// `LifecycleDeletion` notifications) and aborts stale multipart uploads.
/// Returns `(objects deleted, uploads aborted)`.
pub async fn apply(db: &Db) -> Result<(usize, usize)> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let (mut deleted, mut aborted) = (0, 0);
    for (bucket, stored) in db.r2_lifecycles().await? {
        let rules = match parse_rules(&stored.unwrap_or_else(default_rules)) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%bucket, error = %e, "skipping a bucket with invalid lifecycle rules");
                continue;
            }
        };
        for r in rules.iter().filter(|r| r.enabled) {
            let before = match r.delete {
                Some(Condition::Age(age)) => Some(now_ms - age * 1000),
                Some(Condition::Date(at)) if now_ms >= at => Some(i64::MAX),
                _ => None,
            };
            if let Some(before) = before {
                let keys = db.r2_keys_uploaded_before(&bucket, &r.prefix, before, BATCH).await?;
                if !keys.is_empty() {
                    deleted += db.r2_delete_as(&bucket, &keys, "LifecycleDeletion").await?.len();
                }
            }
            if let Some(age) = r.abort_uploads_after {
                for upload in db.r2_uploads_older_than(&bucket, &r.prefix, age, BATCH).await? {
                    db.r2_abort_upload(&upload).await?;
                    aborted += 1;
                }
            }
        }
    }
    Ok((deleted, aborted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_parse_as_wrangler_sends_them() {
        let rules = parse_rules(&json!([
            { "id": "old", "enabled": true, "conditions": { "prefix": "logs/" },
              "deleteObjectsTransition": { "condition": { "maxAge": 2_592_000, "type": "Age" } } },
            { "id": "y2k", "enabled": false, "conditions": {},
              "deleteObjectsTransition": { "condition": { "date": "2025-01-01T00:00:00.000Z", "type": "Date" } },
              "storageClassTransitions": [] },
        ]))
        .unwrap();
        assert_eq!(rules[0].prefix, "logs/");
        assert_eq!(rules[0].delete, Some(Condition::Age(2_592_000)));
        assert!(!rules[1].enabled);
        assert_eq!(rules[1].delete, Some(Condition::Date(1_735_689_600_000)));
        assert_eq!(parse_rules(&default_rules()).unwrap()[0].abort_uploads_after, Some(604_800));
    }

    #[test]
    fn bad_rules_are_refused() {
        assert!(parse_rules(&json!({})).is_err());
        assert!(parse_rules(&json!([{ "conditions": {} }])).is_err());
        assert!(parse_rules(&json!([{ "id": "a" }, { "id": "a" }])).is_err());
        assert!(
            parse_rules(
                &json!([{ "id": "a", "deleteObjectsTransition": { "condition": { "type": "Age" } } }])
            )
            .is_err()
        );
        assert!(
            parse_rules(&json!([{ "id": "a", "abortMultipartUploadsTransition":
            { "condition": { "type": "Date", "date": "2025-01-01T00:00:00Z" } } }]))
            .is_err()
        );
    }
}
