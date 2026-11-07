//! `wrangler tail`: trace events from runners to the WebSockets that asked for
//! them.
//!
//! Each account process has a tail worker (`selvedge-tail`) that hands every
//! script's trace events to its agent, which forwards only those of scripts
//! with a live session (`POST /internal/v1/accounts/:id/tail-events`). The API
//! replica that receives them applies each session's filters and publishes
//! the matches with `NOTIFY`; the replica holding the session's WebSocket
//! `LISTEN`s and sends them on. Delivery is best effort, as on Cloudflare.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use selvedge_core::AccountId;
use selvedge_store::Db;
use selvedge_store::tails::{MAX_NOTIFY_BYTES, TAIL_CHANNEL, TailSession};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const SESSION_CACHE_TTL: Duration = Duration::from_secs(1);

type SessionCache = HashMap<(AccountId, String), (Instant, Arc<Vec<TailSession>>)>;

pub struct Hub {
    db: Db,
    /// Session id to the WebSockets on this replica.
    subscribers: Mutex<HashMap<String, Vec<mpsc::Sender<String>>>>,
    sessions: Mutex<SessionCache>,
}

impl Hub {
    /// Starts listening for published events.
    pub fn start(db: Db) -> Arc<Self> {
        let hub =
            Arc::new(Self { db: db.clone(), subscribers: Mutex::default(), sessions: Mutex::default() });
        let mut rx = db.listen(TAIL_CHANNEL);
        let weak = Arc::downgrade(&hub);
        tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                let Some(hub) = weak.upgrade() else { break };
                if let Some((id, event)) = payload.split_once('\n') {
                    hub.deliver(id, event);
                }
            }
        });
        hub
    }

    fn deliver(&self, id: &str, event: &str) {
        let mut subs = self.subscribers.lock().expect("poisoned");
        if let Some(list) = subs.get_mut(id) {
            // A slow client loses events instead of holding up the others.
            list.retain(|tx| {
                !matches!(tx.try_send(event.to_owned()), Err(mpsc::error::TrySendError::Closed(_)))
            });
            if list.is_empty() {
                subs.remove(id);
            }
        }
    }

    pub fn subscribe(&self, id: &str) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel(256);
        self.subscribers.lock().expect("poisoned").entry(id.to_owned()).or_default().push(tx);
        rx
    }

    async fn sessions(&self, account: &AccountId, script: &str) -> Result<Arc<Vec<TailSession>>> {
        let key = (account.clone(), script.to_owned());
        if let Some((at, s)) = self.sessions.lock().expect("poisoned").get(&key)
            && at.elapsed() < SESSION_CACHE_TTL
        {
            return Ok(s.clone());
        }
        let s = Arc::new(self.db.tails(account, script).await?);
        let mut cache = self.sessions.lock().expect("poisoned");
        cache.retain(|_, (at, _)| at.elapsed() < SESSION_CACHE_TTL);
        cache.insert(key, (Instant::now(), s.clone()));
        Ok(s)
    }

    /// Publishes a script's trace events to the sessions whose filters match.
    pub async fn publish(&self, account: &AccountId, script: &str, events: Vec<Value>) -> Result<()> {
        let sessions = self.sessions(account, script).await?;
        for event in events {
            let mut text = None;
            for s in sessions.iter() {
                if !matches(&s.filters, s.creator_ip.as_deref(), &event) {
                    continue;
                }
                let text = text.get_or_insert_with(|| fit(event.clone(), MAX_NOTIFY_BYTES - 40));
                self.db.notify(TAIL_CHANNEL, &format!("{}\n{text}", s.id)).await?;
            }
        }
        Ok(())
    }
}

/// Whether an event passes every filter of a session (the `filters` list
/// wrangler sends: sampling_rate, outcome, method, header, client_ip, query).
pub fn matches(filters: &Value, creator_ip: Option<&str>, event: &Value) -> bool {
    let request = &event["event"]["request"];
    filters.as_array().into_iter().flatten().all(|f| {
        if let Some(rate) = f["sampling_rate"].as_f64() {
            return rand::random::<f64>() < rate;
        }
        if let Some(outcomes) = f["outcome"].as_array() {
            return outcomes.iter().any(|o| o.as_str() == event["outcome"].as_str());
        }
        if let Some(methods) = f["method"].as_array() {
            let m = request["method"].as_str().unwrap_or_default();
            return methods.iter().filter_map(Value::as_str).any(|x| x.eq_ignore_ascii_case(m));
        }
        if let Some(h) = f.get("header").filter(|h| h.is_object()) {
            let key = h["key"].as_str().unwrap_or_default().to_ascii_lowercase();
            let value = request["headers"].as_object().and_then(|hs| {
                hs.iter().find(|(k, _)| k.eq_ignore_ascii_case(&key)).and_then(|(_, v)| v.as_str())
            });
            return match (value, h["query"].as_str()) {
                (Some(v), Some(q)) => v.contains(q),
                (Some(_), None) => true,
                (None, _) => false,
            };
        }
        if let Some(ips) = f["client_ip"].as_array() {
            let ip = request["cf"]["clientIp"].as_str().unwrap_or_default();
            return ips.iter().filter_map(Value::as_str).any(|x| match x {
                "self" => creator_ip == Some(ip),
                other => other == ip,
            });
        }
        if let Some(q) = f["query"].as_str() {
            let haystack = format!("{}{}", event["logs"], event["exceptions"]);
            return haystack.contains(q);
        }
        // Unknown filters (such as a version) do not exclude events.
        true
    })
}

fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Serializes an event in at most `max` bytes, shortening logs, exceptions and
/// headers (and setting `truncated`) when it is larger.
pub fn fit(mut event: Value, max: usize) -> String {
    let text = event.to_string();
    if text.len() <= max {
        return text;
    }
    event["truncated"] = json!(true);
    for limit in [2048, 512, 128, 32] {
        if let Some(logs) = event["logs"].as_array_mut() {
            for log in logs {
                let joined = match &log["message"] {
                    Value::Array(parts) => parts
                        .iter()
                        .map(|p| p.as_str().map_or_else(|| p.to_string(), str::to_owned))
                        .collect::<Vec<_>>()
                        .join(" "),
                    other => other.to_string(),
                };
                log["message"] = json!([cut(&joined, limit)]);
            }
        }
        if let Some(exceptions) = event["exceptions"].as_array_mut() {
            for e in exceptions {
                for field in ["message", "stack"] {
                    if let Some(s) = e[field].as_str() {
                        e[field] = json!(cut(s, limit));
                    }
                }
            }
        }
        if let Some(headers) = event["event"]["request"]["headers"].as_object_mut() {
            for v in headers.values_mut() {
                if let Some(s) = v.as_str() {
                    *v = json!(cut(s, limit));
                }
            }
        }
        if let Some(url) = event["event"]["request"]["url"].as_str() {
            event["event"]["request"]["url"] = json!(cut(url, limit.max(256)));
        }
        let text = event.to_string();
        if text.len() <= max {
            return text;
        }
    }
    // Still too large (very many log lines): keep the event, drop the details.
    for (key, empty) in [("logs", json!([])), ("exceptions", json!([]))] {
        event[key] = empty;
        let text = event.to_string();
        if text.len() <= max {
            return text;
        }
    }
    if event["event"]["request"].is_object() {
        event["event"]["request"]["headers"] = json!({});
    }
    let text = event.to_string();
    if text.len() <= max {
        text
    } else {
        json!({ "outcome": event["outcome"], "truncated": true }).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Value {
        json!({
            "outcome": "ok",
            "logs": [{ "message": ["hello", { "a": 1 }], "level": "log", "timestamp": 1 }],
            "exceptions": [],
            "event": { "request": {
                "url": "https://x/a", "method": "GET",
                "headers": { "user-agent": "curl/8", "cf-connecting-ip": "1.2.3.4" },
                "cf": { "clientIp": "1.2.3.4" }
            }}
        })
    }

    #[test]
    fn filters() {
        let e = event();
        assert!(matches(&json!([]), None, &e));
        assert!(matches(&json!([{ "outcome": ["ok"] }]), None, &e));
        assert!(!matches(&json!([{ "outcome": ["exception"] }]), None, &e));
        assert!(matches(&json!([{ "method": ["get"] }]), None, &e));
        assert!(!matches(&json!([{ "method": ["POST"] }]), None, &e));
        assert!(matches(&json!([{ "header": { "key": "User-Agent", "query": "curl" } }]), None, &e));
        assert!(!matches(&json!([{ "header": { "key": "x-missing" } }]), None, &e));
        assert!(matches(&json!([{ "client_ip": ["self"] }]), Some("1.2.3.4"), &e));
        assert!(!matches(&json!([{ "client_ip": ["5.6.7.8"] }]), None, &e));
        assert!(matches(&json!([{ "query": "hello" }]), None, &e));
        assert!(!matches(&json!([{ "query": "absent" }, { "outcome": ["ok"] }]), None, &e));
        assert!(matches(&json!([{ "scriptVersion": "x" }]), None, &e));
    }

    #[test]
    fn fitting() {
        let e = event();
        assert_eq!(fit(e.clone(), 10_000), e.to_string());
        let mut big = event();
        big["logs"] = json!(
            (0..50).map(|i| json!({ "message": ["x".repeat(1000), i], "level": "log" })).collect::<Vec<_>>()
        );
        let text = fit(big, 7000);
        assert!(text.len() <= 7000, "{}", text.len());
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["truncated"], true);
        assert_eq!(v["event"]["request"]["method"], "GET");
    }
}
