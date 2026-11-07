//! Zones and Worker routes.

use anyhow::Result;
use selvedge_core::{AccountId, HexId};
use tokio_postgres::error::SqlState;

use crate::Db;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Zone {
    pub id: String,
    pub account_id: AccountId,
    pub name: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerRoute {
    pub id: String,
    pub pattern: String,
    pub script: Option<String>,
}

/// A route pattern for the gateway: requests whose host and path match go to
/// `service` of `account_id` (none if empty).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PatternRoute {
    pub pattern: String,
    pub account_id: AccountId,
    pub service: String,
}

fn zone(r: &tokio_postgres::Row) -> Result<Zone> {
    Ok(Zone { id: r.get(0), account_id: r.get::<_, String>(1).parse()?, name: r.get(2) })
}

fn route(r: &tokio_postgres::Row) -> WorkerRoute {
    WorkerRoute { id: r.get(0), pattern: r.get(1), script: r.get(2) }
}

impl Db {
    /// Assigns a zone to an account; `None` if another zone has the name.
    pub async fn create_zone(&self, account: &AccountId, name: &str) -> Result<Option<Zone>> {
        let r = self
            .client()
            .await?
            .query_one(
                "INSERT INTO zones (id, account_id, name) VALUES ($1, $2, $3) RETURNING id, account_id, name",
                &[&HexId::random().as_str(), &account.as_str(), &name],
            )
            .await;
        match r {
            Ok(row) => Ok(Some(zone(&row)?)),
            Err(e) if e.code() == Some(&SqlState::UNIQUE_VIOLATION) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Zones, optionally of one account and with one name.
    pub async fn zones(&self, account: Option<&AccountId>, name: Option<&str>) -> Result<Vec<Zone>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT id, account_id, name FROM zones
                 WHERE ($1::text IS NULL OR account_id = $1) AND ($2::text IS NULL OR name = $2) ORDER BY name",
                &[&account.map(AccountId::as_str), &name],
            )
            .await?;
        rows.iter().map(zone).collect()
    }

    pub async fn zone(&self, id: &str) -> Result<Option<Zone>> {
        let row = self
            .client()
            .await?
            .query_opt("SELECT id, account_id, name FROM zones WHERE id = $1", &[&id])
            .await?;
        row.as_ref().map(zone).transpose()
    }

    pub async fn delete_zone(&self, id: &str) -> Result<bool> {
        Ok(self.client().await?.execute("DELETE FROM zones WHERE id = $1", &[&id]).await? > 0)
    }

    pub async fn worker_routes(&self, zone: &str) -> Result<Vec<WorkerRoute>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT id, pattern, script FROM worker_routes WHERE zone_id = $1 ORDER BY pattern",
                &[&zone],
            )
            .await?;
        Ok(rows.iter().map(route).collect())
    }

    /// Adds a route; `None` if the zone already has the pattern.
    pub async fn create_worker_route(
        &self,
        zone: &str,
        pattern: &str,
        script: Option<&str>,
    ) -> Result<Option<WorkerRoute>> {
        let r = self
            .client()
            .await?
            .query_one(
                "INSERT INTO worker_routes (id, zone_id, pattern, script) VALUES ($1, $2, $3, $4)
                 RETURNING id, pattern, script",
                &[&HexId::random().as_str(), &zone, &pattern, &script],
            )
            .await;
        match r {
            Ok(row) => Ok(Some(route(&row))),
            Err(e) if e.code() == Some(&SqlState::UNIQUE_VIOLATION) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn update_worker_route(
        &self,
        zone: &str,
        id: &str,
        pattern: &str,
        script: Option<&str>,
    ) -> Result<Option<WorkerRoute>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "UPDATE worker_routes SET pattern = $3, script = $4 WHERE zone_id = $1 AND id = $2
                 RETURNING id, pattern, script",
                &[&zone, &id, &pattern, &script],
            )
            .await?;
        Ok(row.as_ref().map(route))
    }

    pub async fn delete_worker_route(&self, zone: &str, id: &str) -> Result<bool> {
        Ok(self
            .client()
            .await?
            .execute("DELETE FROM worker_routes WHERE zone_id = $1 AND id = $2", &[&zone, &id])
            .await?
            > 0)
    }

    /// A script's routes in the account's zones.
    pub async fn script_routes(&self, account: &AccountId, script: &str) -> Result<Vec<WorkerRoute>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT r.id, r.pattern, r.script FROM worker_routes r JOIN zones z ON z.id = r.zone_id
                 WHERE z.account_id = $1 AND r.script = $2 ORDER BY r.pattern",
                &[&account.as_str(), &script],
            )
            .await?;
        Ok(rows.iter().map(route).collect())
    }

    /// Replaces a script's routes with `(zone id, pattern)`s, in one
    /// transaction. `Ok(Err(pattern))` if a pattern is taken by another
    /// script's route.
    pub async fn replace_script_routes(
        &self,
        account: &AccountId,
        script: &str,
        routes: &[(String, String)],
    ) -> Result<std::result::Result<Vec<WorkerRoute>, String>> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "DELETE FROM worker_routes r USING zones z
             WHERE z.id = r.zone_id AND z.account_id = $1 AND r.script = $2",
            &[&account.as_str(), &script],
        )
        .await?;
        let mut out = Vec::new();
        for (zone, pattern) in routes {
            let row = tx
                .query_opt(
                    "INSERT INTO worker_routes (id, zone_id, pattern, script) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (zone_id, pattern) DO NOTHING RETURNING id, pattern, script",
                    &[&HexId::random().as_str(), zone, pattern, &script],
                )
                .await?;
            match row {
                Some(r) => out.push(route(&r)),
                None => return Ok(Err(pattern.clone())),
            }
        }
        tx.commit().await?;
        Ok(Ok(out))
    }

    /// Every route, for the gateway. A route without a script has an empty
    /// service: matching requests skip Workers.
    pub async fn pattern_routes(&self) -> Result<Vec<PatternRoute>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT r.pattern, z.account_id, coalesce(r.script, '') FROM worker_routes r
                 JOIN zones z ON z.id = r.zone_id ORDER BY r.pattern",
                &[],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(PatternRoute {
                    pattern: r.get(0),
                    account_id: r.get::<_, String>(1).parse()?,
                    service: r.get(2),
                })
            })
            .collect()
    }
}
