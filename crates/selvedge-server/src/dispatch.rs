//! Event delivery: Cron Triggers (and, later, queue batches) to the account
//! processes, through the gateway's internal routing and each account's
//! `selvedge-events` system worker.
//!
//! Runs in every data service replica. Due firings are claimed in the
//! database with SKIP LOCKED, so replicas share the work and each firing is
//! delivered once (Cloudflare does not retry a failed Cron Trigger either).

use std::sync::Arc;
use std::time::Duration;

use selvedge_store::Db;
use selvedge_store::cron::Firing;
use serde_json::json;

use crate::agent::{ACCOUNT_HEADER, SCRIPT_HEADER, TOKEN_HEADER};

pub struct Dispatcher {
    pub db: Db,
    pub gateway_url: String,
    pub internal_token: String,
    pub http: reqwest::Client,
}

impl Dispatcher {
    pub fn new(db: Db, gateway_url: String, internal_token: String) -> Arc<Self> {
        Arc::new(Self {
            db,
            gateway_url: gateway_url.trim_end_matches('/').to_owned(),
            internal_token,
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                // A scheduled handler may run up to 15 minutes on Cloudflare.
                .timeout(Duration::from_secs(15 * 60))
                .build()
                .expect("HTTP client"),
        })
    }

    pub async fn run_cron(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            match self.db.claim_due_firings(200).await {
                Ok(firings) => {
                    for f in firings {
                        let me = self.clone();
                        tokio::spawn(async move { me.deliver_scheduled(f).await });
                    }
                }
                Err(e) => tracing::warn!(error = %format!("{e:#}"), "claiming Cron Triggers failed"),
            }
        }
    }

    async fn deliver_scheduled(&self, f: Firing) {
        let resp = self
            .http
            .post(format!("{}/scheduled", self.gateway_url))
            .header(TOKEN_HEADER, &self.internal_token)
            .header(ACCOUNT_HEADER, f.account.as_str())
            .header(SCRIPT_HEADER, selvedge_workerd::render::EVENTS_SERVICE)
            .json(&json!({ "script": f.script, "cron": f.cron, "scheduledTime": f.scheduled_ms }))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let outcome = r
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| v["outcome"].as_str().map(str::to_owned));
                tracing::info!(account = %f.account, script = %f.script, cron = %f.cron, outcome = ?outcome, "Cron Trigger ran");
            }
            Ok(r) => {
                tracing::warn!(account = %f.account, script = %f.script, status = %r.status(), "Cron Trigger delivery failed")
            }
            Err(e) => {
                tracing::warn!(account = %f.account, script = %f.script, error = %e, "Cron Trigger delivery failed")
            }
        }
    }
}
