//! Queries. Each function is one transaction.

use anyhow::{Context, Result};
use selvedge_core::{AccountBundle, AccountId, Binding, BundleScript, HexId, Module, NamespaceId, VersionId};
use serde_json::Value;

use crate::{Db, SecretBox};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Admin,
    Account(AccountId),
}

impl Principal {
    pub fn may_access(&self, account: &AccountId) -> bool {
        match self {
            Self::Admin => true,
            Self::Account(a) => a == account,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub subdomain: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScriptInfo {
    pub id: String,
    pub current_version_id: Option<String>,
    pub workers_dev: bool,
    pub created_on: String,
    pub modified_on: String,
}

#[derive(Debug, Clone)]
pub struct NewVersion {
    pub main_module: String,
    pub compatibility_date: String,
    pub compatibility_flags: Vec<String>,
    pub modules: Vec<Module>,
    pub bindings: Vec<Binding>,
    /// Binding names to carry over from the current version.
    pub keep_bindings: Vec<String>,
    /// `secret_text` bindings uploaded with the script: (name, text).
    pub secrets: Vec<(String, String)>,
    pub assets: Option<selvedge_core::assets::ScriptAssets>,
    /// Carry over the current version's assets when `assets` is `None`.
    pub keep_assets: bool,
}

#[derive(Debug, Clone)]
pub struct VersionInfo {
    pub id: String,
    pub number: i64,
    pub created_on: String,
    pub main_module: String,
    pub compatibility_date: String,
    pub compatibility_flags: Vec<String>,
    pub bindings: Vec<Binding>,
    pub has_assets: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct D1Info {
    pub id: String,
    pub name: String,
    pub created_on: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KvNamespace {
    pub id: NamespaceId,
    pub title: String,
}

#[derive(Debug, Clone)]
pub struct KvEntry {
    pub value: Vec<u8>,
    pub metadata: Option<String>,
    pub expiration: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct KvKey {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiration: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct Route {
    pub hostname: String,
    pub account_id: AccountId,
    pub service: String,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// A timestamp formatted like every other API timestamp.
pub fn rfc3339(t: std::time::SystemTime) -> String {
    ts(t)
}

/// The current time, formatted like every other API timestamp.
pub fn now_rfc3339() -> String {
    ts(std::time::SystemTime::now())
}

fn ts(t: std::time::SystemTime) -> String {
    // RFC 3339 in UTC without pulling in a date library.
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000000Z", rem / 3600, (rem / 60) % 60, rem % 60)
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn token_hash(token: &str) -> String {
    selvedge_core::sha256_hex(token.as_bytes())
}

fn secret_aad(account: &AccountId, script: &str, name: &str) -> String {
    format!("{account}/{script}/{name}")
}

impl Db {
    // ---- accounts and tokens ----

    pub async fn create_account(&self, name: &str) -> Result<Account> {
        let id = HexId::random();
        // The default workers.dev-style subdomain is derived from the id, so it is unique.
        let subdomain = format!("a{}", &id.as_str()[..12]);
        self.client()
            .await?
            .execute(
                "INSERT INTO accounts (id, name, subdomain) VALUES ($1, $2, $3)",
                &[&id.as_str(), &name, &subdomain],
            )
            .await?;
        Ok(Account { id, name: name.to_owned(), subdomain: Some(subdomain) })
    }

    pub async fn account(&self, id: &AccountId) -> Result<Option<Account>> {
        let row = self
            .client()
            .await?
            .query_opt("SELECT name, subdomain FROM accounts WHERE id = $1", &[&id.as_str()])
            .await?;
        Ok(row.map(|r| Account { id: id.clone(), name: r.get(0), subdomain: r.get(1) }))
    }

    pub async fn accounts(&self, principal: &Principal) -> Result<Vec<Account>> {
        let client = self.client().await?;
        let rows = match principal {
            Principal::Admin => {
                client.query("SELECT id, name, subdomain FROM accounts ORDER BY created_at", &[]).await?
            }
            Principal::Account(a) => {
                client.query("SELECT id, name, subdomain FROM accounts WHERE id = $1", &[&a.as_str()]).await?
            }
        };
        rows.iter()
            .map(|r| Ok(Account { id: r.get::<_, String>(0).parse()?, name: r.get(1), subdomain: r.get(2) }))
            .collect()
    }

    pub async fn set_subdomain(&self, account: &AccountId, subdomain: &str) -> Result<bool> {
        let n = self
            .client()
            .await?
            .execute("UPDATE accounts SET subdomain = $2 WHERE id = $1", &[&account.as_str(), &subdomain])
            .await;
        match n {
            Ok(n) => Ok(n == 1),
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Creates an API token and returns its plaintext, which is not stored.
    /// Creates a token. Returns its id and its value.
    pub async fn create_token(&self, account: Option<&AccountId>, name: &str) -> Result<(String, String)> {
        let bytes: [u8; 24] = rand::random();
        let token = format!("slv_{}", hex::encode(bytes));
        let id = HexId::random().to_string();
        self.client()
            .await?
            .execute(
                "INSERT INTO api_tokens (token_sha256, account_id, name, id) VALUES ($1, $2, $3, $4)",
                &[&token_hash(&token), &account.map(HexId::as_str), &name, &id],
            )
            .await?;
        Ok((id, token))
    }

    /// For R2's S3 API: an account token's secret (the SHA-256 of its value)
    /// and account, by the token's id. Admin tokens have no account and no S3
    /// access.
    pub async fn s3_credentials(&self, access_key_id: &str) -> Result<Option<(String, AccountId)>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT token_sha256, account_id FROM api_tokens WHERE id = $1 AND account_id IS NOT NULL",
                &[&access_key_id],
            )
            .await?;
        row.map(|r| Ok((r.get(0), r.get::<_, String>(1).parse()?))).transpose()
    }

    /// Ensures an admin token with this plaintext exists (bootstrap from config).
    pub async fn ensure_admin_token(&self, token: &str) -> Result<()> {
        self.client()
            .await?
            .execute(
                "INSERT INTO api_tokens (token_sha256, account_id, name, id) VALUES ($1, NULL, 'bootstrap', md5(random()::text))
                 ON CONFLICT (token_sha256) DO NOTHING",
                &[&token_hash(token)],
            )
            .await?;
        Ok(())
    }

    pub async fn verify_token(&self, token: &str) -> Result<Option<Principal>> {
        let row = self
            .client()
            .await?
            .query_opt("SELECT account_id FROM api_tokens WHERE token_sha256 = $1", &[&token_hash(token)])
            .await?;
        Ok(match row {
            None => None,
            Some(r) => match r.get::<_, Option<String>>(0) {
                None => Some(Principal::Admin),
                Some(a) => Some(Principal::Account(a.parse()?)),
            },
        })
    }

    // ---- scripts ----

    /// Stores a new version and makes it current. Returns its id.
    pub async fn put_version(
        &self,
        secrets: &SecretBox,
        account: &AccountId,
        script: &str,
        mut v: NewVersion,
        make_current: bool,
    ) -> Result<(VersionId, ScriptInfo)> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "INSERT INTO scripts (account_id, name) VALUES ($1, $2)
             ON CONFLICT (account_id, name) DO UPDATE SET updated_at = now()",
            &[&account.as_str(), &script],
        )
        .await?;
        if v.keep_assets && v.assets.is_none() {
            let current = tx
                .query_opt(
                    "SELECT v.assets FROM scripts s JOIN script_versions v ON v.id = s.current_version_id
                     WHERE s.account_id = $1 AND s.name = $2",
                    &[&account.as_str(), &script],
                )
                .await?;
            if let Some(Some(a)) = current.map(|r| r.get::<_, Option<serde_json::Value>>(0)) {
                v.assets = Some(serde_json::from_value(a)?);
            }
        }
        if !v.keep_bindings.is_empty() {
            let current = tx
                .query_opt(
                    "SELECT v.bindings FROM scripts s JOIN script_versions v ON v.id = s.current_version_id
                     WHERE s.account_id = $1 AND s.name = $2 FOR UPDATE OF s",
                    &[&account.as_str(), &script],
                )
                .await?;
            if let Some(row) = current {
                let old: Vec<Binding> = serde_json::from_value(row.get(0))?;
                for b in old {
                    let keep = v.keep_bindings.iter().any(|k| k == b.name() || k == binding_type(&b));
                    if keep && !v.bindings.iter().any(|n| n.name() == b.name()) {
                        v.bindings.push(b);
                    }
                }
            }
        }
        for (name, text) in &v.secrets {
            let sealed = secrets.seal(text.as_bytes(), &secret_aad(account, script, name))?;
            tx.execute(
                "INSERT INTO script_secrets (account_id, script_name, name, ciphertext) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (account_id, script_name, name) DO UPDATE SET ciphertext = excluded.ciphertext",
                &[&account.as_str(), &script, name, &sealed],
            )
            .await?;
        }
        let id = selvedge_core::new_version_id();
        tx.execute(
            "INSERT INTO script_versions
               (id, account_id, script_name, main_module, compatibility_date, compatibility_flags, modules, bindings,
                assets)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[
                &id,
                &account.as_str(),
                &script,
                &v.main_module,
                &v.compatibility_date,
                &serde_json::to_value(&v.compatibility_flags)?,
                &serde_json::to_value(&v.modules)?,
                &serde_json::to_value(&v.bindings)?,
                &v.assets.as_ref().map(serde_json::to_value).transpose()?,
            ],
        )
        .await?;
        if make_current {
            tx.execute(
                "UPDATE scripts SET current_version_id = $3, split = NULL, updated_at = now()
                 WHERE account_id = $1 AND name = $2",
                &[&account.as_str(), &script, &id],
            )
            .await?;
            bump_generation(&tx, account).await?;
        }
        let info = script_info(&*tx, account, script).await?.context("script vanished")?;
        tx.commit().await?;
        Ok((id, info))
    }

    pub async fn scripts(&self, account: &AccountId) -> Result<Vec<ScriptInfo>> {
        let client = self.client().await?;
        let rows = client
            .query(
                "SELECT name, current_version_id, workers_dev, created_at, updated_at
                 FROM scripts WHERE account_id = $1 ORDER BY name",
                &[&account.as_str()],
            )
            .await?;
        Ok(rows.iter().map(row_to_script).collect())
    }

    pub async fn script(&self, account: &AccountId, script: &str) -> Result<Option<ScriptInfo>> {
        let client = self.client().await?;
        script_info(&**client, account, script).await
    }

    /// The current version's modules, for `GET .../scripts/:name/content`.
    pub async fn current_modules(
        &self,
        account: &AccountId,
        script: &str,
    ) -> Result<Option<(String, Vec<Module>)>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT v.main_module, v.modules FROM scripts s JOIN script_versions v ON v.id = s.current_version_id
                 WHERE s.account_id = $1 AND s.name = $2",
                &[&account.as_str(), &script],
            )
            .await?;
        row.map(|r| Ok((r.get(0), serde_json::from_value(r.get(1))?))).transpose()
    }

    pub async fn current_bindings(&self, account: &AccountId, script: &str) -> Result<Option<Vec<Binding>>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT v.bindings FROM scripts s JOIN script_versions v ON v.id = s.current_version_id
                 WHERE s.account_id = $1 AND s.name = $2",
                &[&account.as_str(), &script],
            )
            .await?;
        row.map(|r| Ok(serde_json::from_value(r.get(0))?)).transpose()
    }

    pub async fn set_workers_dev(&self, account: &AccountId, script: &str, enabled: bool) -> Result<bool> {
        let n = self
            .client()
            .await?
            .execute(
                "UPDATE scripts SET workers_dev = $3 WHERE account_id = $1 AND name = $2",
                &[&account.as_str(), &script, &enabled],
            )
            .await?;
        Ok(n == 1)
    }

    pub async fn delete_script(&self, account: &AccountId, script: &str) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let n = tx
            .execute("DELETE FROM scripts WHERE account_id = $1 AND name = $2", &[&account.as_str(), &script])
            .await?;
        tx.execute(
            "DELETE FROM worker_domains WHERE account_id = $1 AND service = $2",
            &[&account.as_str(), &script],
        )
        .await?;
        if n > 0 {
            bump_generation(&tx, account).await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    pub async fn versions(&self, account: &AccountId, script: &str) -> Result<Vec<VersionInfo>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT id, created_at, main_module, compatibility_date, compatibility_flags, bindings,
                        row_number() OVER (ORDER BY created_at, id), assets IS NOT NULL
                 FROM script_versions WHERE account_id = $1 AND script_name = $2
                 ORDER BY created_at DESC, id DESC LIMIT 100",
                &[&account.as_str(), &script],
            )
            .await?;
        rows.iter().map(row_to_version).collect()
    }

    pub async fn version(&self, account: &AccountId, script: &str, id: &str) -> Result<Option<VersionInfo>> {
        Ok(self.versions(account, script).await?.into_iter().find(|v| v.id == id))
    }

    /// A gradual deployment's versions and percentages, if one is in place.
    pub async fn split(&self, account: &AccountId, script: &str) -> Result<Option<Vec<(String, f64)>>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT split FROM scripts WHERE account_id = $1 AND name = $2",
                &[&account.as_str(), &script],
            )
            .await?;
        Ok(row
            .and_then(|r| r.get::<_, Option<serde_json::Value>>(0))
            .and_then(|v| serde_json::from_value(v).ok()))
    }

    /// Splits requests between two versions; the first, with the larger
    /// share, becomes current. `false` if either version does not exist.
    pub async fn set_split(
        &self,
        account: &AccountId,
        script: &str,
        versions: &[(String, f64)],
    ) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let found = tx
            .query_one(
                "SELECT count(*) FROM script_versions WHERE account_id = $1 AND script_name = $2 AND id = ANY($3)",
                &[&account.as_str(), &script, &versions.iter().map(|(v, _)| v.clone()).collect::<Vec<_>>()],
            )
            .await?
            .get::<_, i64>(0);
        if found as usize != versions.len() {
            return Ok(false);
        }
        let n = tx
            .execute(
                "UPDATE scripts SET current_version_id = $3, split = $4, updated_at = now()
                 WHERE account_id = $1 AND name = $2",
                &[&account.as_str(), &script, &versions[0].0, &serde_json::to_value(versions)?],
            )
            .await?;
        if n == 1 {
            bump_generation(&tx, account).await?;
        }
        tx.commit().await?;
        Ok(n == 1)
    }

    /// Makes an existing version current (deploy or rollback), ending any
    /// gradual deployment.
    pub async fn set_current_version(&self, account: &AccountId, script: &str, id: &str) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let n = tx
            .execute(
                "UPDATE scripts SET current_version_id = $3, split = NULL, updated_at = now()
                 WHERE account_id = $1 AND name = $2
                   AND EXISTS (SELECT 1 FROM script_versions v WHERE v.id = $3 AND v.account_id = $1 AND v.script_name = $2)",
                &[&account.as_str(), &script, &id],
            )
            .await?;
        if n == 1 {
            bump_generation(&tx, account).await?;
        }
        tx.commit().await?;
        Ok(n == 1)
    }

    // ---- secrets ----

    /// Sets a secret and adds a `secret_text` binding for it to a new version.
    pub async fn put_secret(
        &self,
        secrets: &SecretBox,
        account: &AccountId,
        script: &str,
        name: &str,
        text: &str,
    ) -> Result<bool> {
        let Some(mut bindings) = self.current_bindings(account, script).await? else {
            return Ok(false);
        };
        let Some((main_module, modules)) = self.current_modules(account, script).await? else {
            return Ok(false);
        };
        let (date, flags) = self.current_compat(account, script).await?;
        bindings.retain(|b| b.name() != name);
        bindings.push(Binding::SecretText { name: name.to_owned(), text: None });
        self.put_version(
            secrets,
            account,
            script,
            NewVersion {
                main_module,
                compatibility_date: date,
                compatibility_flags: flags,
                modules,
                bindings,
                keep_bindings: vec![],
                secrets: vec![(name.to_owned(), text.to_owned())],
                assets: None,
                keep_assets: true,
            },
            true,
        )
        .await?;
        Ok(true)
    }

    async fn current_compat(&self, account: &AccountId, script: &str) -> Result<(String, Vec<String>)> {
        let row = self
            .client()
            .await?
            .query_one(
                "SELECT v.compatibility_date, v.compatibility_flags FROM scripts s
                 JOIN script_versions v ON v.id = s.current_version_id
                 WHERE s.account_id = $1 AND s.name = $2",
                &[&account.as_str(), &script],
            )
            .await?;
        Ok((row.get(0), serde_json::from_value(row.get(1))?))
    }

    pub async fn delete_secret(
        &self,
        secrets: &SecretBox,
        account: &AccountId,
        script: &str,
        name: &str,
    ) -> Result<bool> {
        let Some(mut bindings) = self.current_bindings(account, script).await? else {
            return Ok(false);
        };
        let before = bindings.len();
        bindings.retain(|b| !(b.name() == name && matches!(b, Binding::SecretText { .. })));
        if bindings.len() == before {
            return Ok(false);
        }
        let (main_module, modules) = self.current_modules(account, script).await?.context("no version")?;
        let (date, flags) = self.current_compat(account, script).await?;
        self.put_version(
            secrets,
            account,
            script,
            NewVersion {
                main_module,
                compatibility_date: date,
                compatibility_flags: flags,
                modules,
                bindings,
                keep_bindings: vec![],
                secrets: vec![],
                assets: None,
                keep_assets: true,
            },
            true,
        )
        .await?;
        self.client()
            .await?
            .execute(
                "DELETE FROM script_secrets WHERE account_id = $1 AND script_name = $2 AND name = $3",
                &[&account.as_str(), &script, &name],
            )
            .await?;
        Ok(true)
    }

    pub async fn secret_names(&self, account: &AccountId, script: &str) -> Result<Vec<String>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT name FROM script_secrets WHERE account_id = $1 AND script_name = $2 ORDER BY name",
                &[&account.as_str(), &script],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    // ---- bundles (agents) ----

    pub async fn generation(&self, account: &AccountId) -> Result<Option<i64>> {
        let row = self
            .client()
            .await?
            .query_opt("SELECT generation FROM accounts WHERE id = $1", &[&account.as_str()])
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// Accounts whose generation is above `after`, oldest change first.
    pub async fn generations_after(&self, after: i64, limit: i64) -> Result<Vec<(AccountId, i64)>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT id, generation FROM accounts WHERE generation > $1 ORDER BY generation LIMIT $2",
                &[&after, &limit],
            )
            .await?;
        rows.iter().map(|r| Ok((r.get::<_, String>(0).parse()?, r.get(1)))).collect()
    }

    /// The newest generation of any account.
    pub async fn generation_head(&self) -> Result<i64> {
        let row =
            self.client().await?.query_one("SELECT coalesce(max(generation), 0) FROM accounts", &[]).await?;
        Ok(row.get(0))
    }

    /// Every script's current version with secrets decrypted.
    pub async fn bundle(&self, secrets: &SecretBox, account: &AccountId) -> Result<Option<AccountBundle>> {
        let mut client = self.client().await?;
        // One snapshot, so the generation matches the scripts.
        let tx = client
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        let Some(gen_row) =
            tx.query_opt("SELECT generation FROM accounts WHERE id = $1", &[&account.as_str()]).await?
        else {
            return Ok(None);
        };
        let rows = tx
            .query(
                "SELECT s.name, v.id, v.main_module, v.compatibility_date, v.compatibility_flags, v.modules, v.bindings,
                        v.assets
                 FROM scripts s JOIN script_versions v ON v.id = s.current_version_id
                 WHERE s.account_id = $1 ORDER BY s.name",
                &[&account.as_str()],
            )
            .await?;
        // A gradual deployment's other version: split = [[primary, %], [canary, %]].
        let canary_rows = tx
            .query(
                "SELECT s.name, (s.split->1->>1)::float8, v.id, v.main_module, v.compatibility_date,
                        v.compatibility_flags, v.modules, v.bindings, v.assets
                 FROM scripts s JOIN script_versions v ON v.id = s.split->1->>0
                 WHERE s.account_id = $1 AND s.split IS NOT NULL",
                &[&account.as_str()],
            )
            .await?;
        let secret_rows = tx
            .query(
                "SELECT script_name, name, ciphertext FROM script_secrets WHERE account_id = $1",
                &[&account.as_str()],
            )
            .await?;
        // In the snapshot, on this connection: taking a second one from the
        // pool while holding this one deadlocks when many bundles load at once.
        let d1_databases = tx_d1_ids(&*tx, account).await?;
        let workflows = tx
            .query(
                "SELECT name, script_name, class_name FROM workflows WHERE account_id = $1 ORDER BY name",
                &[&account.as_str()],
            )
            .await?
            .iter()
            .map(|r| selvedge_core::WorkflowDef {
                name: r.get(0),
                script_name: r.get(1),
                class_name: r.get(2),
            })
            .collect();
        tx.commit().await?;
        // A version row (columns from `at`: id, main module, date, flags,
        // modules, bindings, assets) as the script `name`, secrets filled in.
        let to_script = |name: &str, r: &tokio_postgres::Row, at: usize| -> Result<BundleScript> {
            let mut bindings: Vec<Binding> = serde_json::from_value(r.get(at + 5))?;
            for b in &mut bindings {
                if let Binding::SecretText { name: secret, text } = b {
                    let row = secret_rows
                        .iter()
                        .find(|s| s.get::<_, &str>(0) == name && s.get::<_, &str>(1) == secret.as_str());
                    if let Some(row) = row {
                        let plain = secrets.open(row.get(2), &secret_aad(account, name, secret))?;
                        *text = Some(String::from_utf8(plain)?);
                    }
                }
            }
            Ok(BundleScript {
                version_id: r.get(at),
                main_module: r.get(at + 1),
                compatibility_date: r.get(at + 2),
                compatibility_flags: serde_json::from_value(r.get(at + 3))?,
                modules: serde_json::from_value(r.get(at + 4))?,
                bindings,
                assets: r
                    .get::<_, Option<serde_json::Value>>(at + 6)
                    .map(serde_json::from_value)
                    .transpose()?,
                canary: None,
                name: name.to_owned(),
            })
        };
        let mut scripts = Vec::new();
        for r in &rows {
            let name: String = r.get(0);
            let mut script = to_script(&name, r, 1)?;
            if let Some(c) = canary_rows.iter().find(|c| c.get::<_, &str>(0) == name) {
                script.canary = Some(Box::new(selvedge_core::Canary {
                    percentage: c.get(1),
                    script: to_script(&name, c, 2)?,
                }));
            }
            scripts.push(script);
        }
        Ok(Some(AccountBundle {
            account_id: account.clone(),
            generation: gen_row.get(0),
            scripts,
            d1_databases,
            workflows,
        }))
    }

    // ---- D1 databases ----

    /// Creates a database; `None` if the name is taken. Bumps the generation so
    /// runners start the D1 backend.
    pub async fn create_d1(&self, account: &AccountId, name: &str) -> Result<Option<D1Info>> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let id = selvedge_core::new_version_id();
        let r = tx
            .query_one(
                "INSERT INTO d1_databases (id, account_id, name) VALUES ($1, $2, $3) RETURNING created_at",
                &[&id, &account.as_str(), &name],
            )
            .await;
        let created = match r {
            Ok(row) => row.get(0),
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => {
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        bump_generation(&tx, account).await?;
        tx.commit().await?;
        Ok(Some(D1Info { id, name: name.to_owned(), created_on: ts(created) }))
    }

    pub async fn d1_databases(&self, account: &AccountId) -> Result<Vec<D1Info>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT id, name, created_at FROM d1_databases WHERE account_id = $1 ORDER BY name",
                &[&account.as_str()],
            )
            .await?;
        Ok(rows.iter().map(|r| D1Info { id: r.get(0), name: r.get(1), created_on: ts(r.get(2)) }).collect())
    }

    /// A database by id or by name.
    pub async fn d1_database(&self, account: &AccountId, id_or_name: &str) -> Result<Option<D1Info>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT id, name, created_at FROM d1_databases WHERE account_id = $1 AND (id = $2 OR name = $2)",
                &[&account.as_str(), &id_or_name],
            )
            .await?;
        Ok(row.map(|r| D1Info { id: r.get(0), name: r.get(1), created_on: ts(r.get(2)) }))
    }

    pub async fn delete_d1(&self, account: &AccountId, id: &str) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let n = tx
            .execute("DELETE FROM d1_databases WHERE account_id = $1 AND id = $2", &[&account.as_str(), &id])
            .await?;
        if n > 0 {
            bump_generation(&tx, account).await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    // ---- KV namespaces ----

    pub async fn create_kv_namespace(&self, account: &AccountId, title: &str) -> Result<Option<KvNamespace>> {
        let id = HexId::random();
        let r = self
            .client()
            .await?
            .execute(
                "INSERT INTO kv_namespaces (id, account_id, title) VALUES ($1, $2, $3)",
                &[&id.as_str(), &account.as_str(), &title],
            )
            .await;
        match r {
            Ok(_) => Ok(Some(KvNamespace { id, title: title.to_owned() })),
            Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn kv_namespaces(&self, account: &AccountId) -> Result<Vec<KvNamespace>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT id, title FROM kv_namespaces WHERE account_id = $1 ORDER BY title",
                &[&account.as_str()],
            )
            .await?;
        rows.iter().map(|r| Ok(KvNamespace { id: r.get::<_, String>(0).parse()?, title: r.get(1) })).collect()
    }

    /// Whether `namespace` belongs to `account`.
    pub async fn kv_namespace_owned(
        &self,
        account: &AccountId,
        namespace: &NamespaceId,
    ) -> Result<Option<KvNamespace>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT title FROM kv_namespaces WHERE id = $1 AND account_id = $2",
                &[&namespace.as_str(), &account.as_str()],
            )
            .await?;
        Ok(row.map(|r| KvNamespace { id: namespace.clone(), title: r.get(0) }))
    }

    pub async fn rename_kv_namespace(
        &self,
        account: &AccountId,
        namespace: &NamespaceId,
        title: &str,
    ) -> Result<bool> {
        let n = self
            .client()
            .await?
            .execute(
                "UPDATE kv_namespaces SET title = $3 WHERE id = $1 AND account_id = $2",
                &[&namespace.as_str(), &account.as_str(), &title],
            )
            .await?;
        Ok(n == 1)
    }

    pub async fn delete_kv_namespace(&self, account: &AccountId, namespace: &NamespaceId) -> Result<bool> {
        let n = self
            .client()
            .await?
            .execute(
                "DELETE FROM kv_namespaces WHERE id = $1 AND account_id = $2",
                &[&namespace.as_str(), &account.as_str()],
            )
            .await?;
        Ok(n == 1)
    }

    // ---- KV entries ----

    pub async fn kv_get(&self, namespace: &NamespaceId, key: &str) -> Result<Option<KvEntry>> {
        let row = self
            .client()
            .await?
            .query_opt(
                "SELECT value, metadata, expiration FROM kv_entries
                 WHERE namespace_id = $1 AND key = $2 AND (expiration IS NULL OR expiration > $3)",
                &[&namespace.as_str(), &key, &now_secs()],
            )
            .await?;
        Ok(row.map(|r| KvEntry { value: r.get(0), metadata: r.get(1), expiration: r.get(2) }))
    }

    pub async fn kv_get_many(
        &self,
        namespace: &NamespaceId,
        keys: &[String],
    ) -> Result<Vec<(String, KvEntry)>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT key, value, metadata, expiration FROM kv_entries
                 WHERE namespace_id = $1 AND key = ANY($2) AND (expiration IS NULL OR expiration > $3)",
                &[&namespace.as_str(), &keys, &now_secs()],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get(0), KvEntry { value: r.get(1), metadata: r.get(2), expiration: r.get(3) }))
            .collect())
    }

    pub async fn kv_put(&self, namespace: &NamespaceId, key: &str, entry: &KvEntry) -> Result<()> {
        self.client()
            .await?
            .execute(
                "INSERT INTO kv_entries (namespace_id, key, value, metadata, expiration) VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (namespace_id, key) DO UPDATE
                 SET value = excluded.value, metadata = excluded.metadata,
                     expiration = excluded.expiration, updated_at = now()",
                &[&namespace.as_str(), &key, &entry.value, &entry.metadata, &entry.expiration],
            )
            .await?;
        Ok(())
    }

    pub async fn kv_delete(&self, namespace: &NamespaceId, keys: &[String]) -> Result<u64> {
        Ok(self
            .client()
            .await?
            .execute(
                "DELETE FROM kv_entries WHERE namespace_id = $1 AND key = ANY($2)",
                &[&namespace.as_str(), &keys],
            )
            .await?)
    }

    /// Lists keys after `after` (exclusive) in byte order. Returns up to
    /// `limit` keys and whether more remain.
    pub async fn kv_list(
        &self,
        namespace: &NamespaceId,
        prefix: &str,
        after: Option<&str>,
        limit: i64,
    ) -> Result<(Vec<KvKey>, bool)> {
        // LIKE needs escaping; compare the prefix byte-wise instead.
        let rows = self
            .client()
            .await?
            .query(
                "SELECT key, expiration, metadata FROM kv_entries
                 WHERE namespace_id = $1 AND left(key, char_length($2)) = $2
                   AND ($3::text IS NULL OR key > $3)
                   AND (expiration IS NULL OR expiration > $4)
                 ORDER BY key LIMIT $5",
                &[&namespace.as_str(), &prefix, &after, &now_secs(), &(limit + 1)],
            )
            .await?;
        let more = rows.len() as i64 > limit;
        let keys = rows
            .iter()
            .take(limit as usize)
            .map(|r| KvKey {
                name: r.get(0),
                expiration: r.get(1),
                metadata: r.get::<_, Option<String>>(2).and_then(|m| serde_json::from_str(&m).ok()),
            })
            .collect();
        Ok((keys, more))
    }

    /// Deletes expired entries. Returns how many.
    pub async fn kv_purge_expired(&self) -> Result<u64> {
        Ok(self
            .client()
            .await?
            .execute("DELETE FROM kv_entries WHERE expiration <= $1", &[&now_secs()])
            .await?)
    }

    // ---- domains and routes ----

    pub async fn put_domain(
        &self,
        account: &AccountId,
        hostname: &str,
        service: &str,
    ) -> Result<Option<String>> {
        let id = HexId::random();
        let row = self
            .client()
            .await?
            .query_opt(
                "INSERT INTO worker_domains (hostname, account_id, service, id) VALUES ($1, $2, $3, $4)
                 ON CONFLICT (hostname) DO UPDATE SET service = excluded.service
                 WHERE worker_domains.account_id = excluded.account_id
                 RETURNING id",
                &[&hostname, &account.as_str(), &service, &id.as_str()],
            )
            .await?;
        // No row: the hostname belongs to another account.
        Ok(row.map(|r| r.get(0)))
    }

    pub async fn domains(&self, account: &AccountId) -> Result<Vec<(String, String, String)>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT id, hostname, service FROM worker_domains WHERE account_id = $1 ORDER BY hostname",
                &[&account.as_str()],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect())
    }

    pub async fn delete_domain(&self, account: &AccountId, id: &str) -> Result<bool> {
        let n = self
            .client()
            .await?
            .execute(
                "DELETE FROM worker_domains WHERE account_id = $1 AND id = $2",
                &[&account.as_str(), &id],
            )
            .await?;
        Ok(n == 1)
    }

    /// The gateway's routing table: custom domains plus `<script>.<subdomain>.<zone>`
    /// for scripts with workers.dev enabled.
    pub async fn routes(&self, workers_dev_zone: Option<&str>) -> Result<Vec<Route>> {
        let client = self.client().await?;
        let mut routes = Vec::new();
        for r in client.query("SELECT hostname, account_id, service FROM worker_domains", &[]).await? {
            routes.push(Route {
                hostname: r.get(0),
                account_id: r.get::<_, String>(1).parse()?,
                service: r.get(2),
            });
        }
        if let Some(zone) = workers_dev_zone {
            let rows = client
                .query(
                    "SELECT s.name, a.id, a.subdomain FROM scripts s JOIN accounts a ON a.id = s.account_id
                     WHERE s.workers_dev AND a.subdomain IS NOT NULL",
                    &[],
                )
                .await?;
            for r in rows {
                let script: String = r.get(0);
                let sub: String = r.get(2);
                routes.push(Route {
                    hostname: format!("{script}.{sub}.{zone}"),
                    account_id: r.get::<_, String>(1).parse()?,
                    service: script,
                });
            }
        }
        Ok(routes)
    }
}

fn binding_type(b: &Binding) -> &'static str {
    match b {
        Binding::PlainText { .. } => "plain_text",
        Binding::SecretText { .. } => "secret_text",
        Binding::Json { .. } => "json",
        Binding::KvNamespace { .. } => "kv_namespace",
        Binding::Service { .. } => "service",
        Binding::DurableObjectNamespace { .. } => "durable_object_namespace",
        Binding::R2Bucket { .. } => "r2_bucket",
        Binding::D1 { .. } => "d1",
        Binding::Queue { .. } => "queue",
        Binding::Assets { .. } => "assets",
        Binding::Workflow { .. } => "workflow",
    }
}

async fn tx_d1_ids<C: tokio_postgres::GenericClient>(client: &C, account: &AccountId) -> Result<Vec<String>> {
    let rows = client
        .query("SELECT id FROM d1_databases WHERE account_id = $1 ORDER BY id", &[&account.as_str()])
        .await?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

async fn bump_generation(tx: &tokio_postgres::Transaction<'_>, account: &AccountId) -> Result<()> {
    tx.execute(
        "UPDATE accounts SET generation = nextval('account_generations') WHERE id = $1",
        &[&account.as_str()],
    )
    .await?;
    Ok(())
}

fn row_to_script(r: &tokio_postgres::Row) -> ScriptInfo {
    ScriptInfo {
        id: r.get(0),
        current_version_id: r.get(1),
        workers_dev: r.get(2),
        created_on: ts(r.get(3)),
        modified_on: ts(r.get(4)),
    }
}

fn row_to_version(r: &tokio_postgres::Row) -> Result<VersionInfo> {
    Ok(VersionInfo {
        id: r.get(0),
        created_on: ts(r.get(1)),
        main_module: r.get(2),
        compatibility_date: r.get(3),
        compatibility_flags: serde_json::from_value(r.get(4))?,
        bindings: serde_json::from_value(r.get(5))?,
        number: r.get(6),
        has_assets: r.get(7),
    })
}

async fn script_info<C: tokio_postgres::GenericClient>(
    client: &C,
    account: &AccountId,
    script: &str,
) -> Result<Option<ScriptInfo>> {
    let row = client
        .query_opt(
            "SELECT name, current_version_id, workers_dev, created_at, updated_at
             FROM scripts WHERE account_id = $1 AND name = $2",
            &[&account.as_str(), &script],
        )
        .await?;
    Ok(row.as_ref().map(row_to_script))
}

#[cfg(test)]
mod tests {
    #[test]
    fn civil_dates() {
        assert_eq!(super::civil_from_days(0), (1970, 1, 1));
        assert_eq!(super::civil_from_days(20_398), (2025, 11, 6));
    }
}
