//! Workflows registered for an account.

use anyhow::Result;
use selvedge_core::AccountId;

use crate::Db;

#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowInfo {
    pub id: String,
    pub name: String,
    pub script_name: String,
    pub class_name: String,
    pub created_on: String,
    pub modified_on: String,
}

const COLUMNS: &str = "id, name, script_name, class_name,
    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),
    to_char(updated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')";

fn row(r: &tokio_postgres::Row) -> WorkflowInfo {
    WorkflowInfo {
        id: r.get(0),
        name: r.get(1),
        script_name: r.get(2),
        class_name: r.get(3),
        created_on: r.get(4),
        modified_on: r.get(5),
    }
}

impl Db {
    /// Creates or updates a workflow. Bumps the generation so runners start
    /// its engine.
    pub async fn put_workflow(
        &self,
        account: &AccountId,
        name: &str,
        script: &str,
        class: &str,
    ) -> Result<WorkflowInfo> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let r = tx
            .query_one(
                &format!(
                    "INSERT INTO workflows (account_id, name, id, script_name, class_name) VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (account_id, name) DO UPDATE
                       SET script_name = excluded.script_name, class_name = excluded.class_name, updated_at = now()
                     RETURNING {COLUMNS}"
                ),
                &[&account.as_str(), &name, &selvedge_core::new_version_id(), &script, &class],
            )
            .await?;
        tx.execute(
            "UPDATE accounts SET generation = nextval('account_generations') WHERE id = $1",
            &[&account.as_str()],
        )
        .await?;
        tx.commit().await?;
        Ok(row(&r))
    }

    pub async fn workflows(&self, account: &AccountId) -> Result<Vec<WorkflowInfo>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!("SELECT {COLUMNS} FROM workflows WHERE account_id = $1 ORDER BY name"),
                &[&account.as_str()],
            )
            .await?;
        Ok(rows.iter().map(row).collect())
    }

    pub async fn workflow(&self, account: &AccountId, name: &str) -> Result<Option<WorkflowInfo>> {
        let r = self
            .client()
            .await?
            .query_opt(
                &format!("SELECT {COLUMNS} FROM workflows WHERE account_id = $1 AND name = $2"),
                &[&account.as_str(), &name],
            )
            .await?;
        Ok(r.as_ref().map(row))
    }

    /// Deletes a workflow; `false` if there was none. Its instances' storage
    /// stays in the bucket.
    pub async fn delete_workflow(&self, account: &AccountId, name: &str) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let n = tx
            .execute("DELETE FROM workflows WHERE account_id = $1 AND name = $2", &[&account.as_str(), &name])
            .await?;
        if n > 0 {
            tx.execute(
                "UPDATE accounts SET generation = nextval('account_generations') WHERE id = $1",
                &[&account.as_str()],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }
}
