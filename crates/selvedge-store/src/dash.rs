//! The web dashboard's people, members and sessions (RFC 0003).

use std::time::Duration;

use anyhow::Result;
use selvedge_core::{AccountId, HexId};

use crate::Db;
use crate::repo::{Principal, rfc3339};

/// Someone who logged in through the OpenID Connect provider, known by the
/// provider's `(iss, sub)`. `admin` and `groups` are as of the last login.
#[derive(Debug, Clone, serde::Serialize)]
pub struct User {
    pub id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub admin: bool,
    #[serde(skip)]
    pub groups: Vec<String>,
    pub created_on: String,
    pub last_login_on: String,
}

/// A member of an account: a user, or an invitation by email.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Member {
    pub id: String,
    pub user: Option<User>,
    /// The invitation's email while `user` is `None`.
    pub email: Option<String>,
    pub created_on: String,
}

/// An account token as the dashboard lists it: never its value.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TokenInfo {
    pub id: String,
    pub name: String,
    pub created_on: String,
    pub created_by: Option<User>,
    /// Whether whoever created it may still use the account, if a user did.
    pub creator_is_member: Option<bool>,
}

/// What a login brought from the provider.
pub struct Login<'a> {
    pub issuer: &'a str,
    pub subject: &'a str,
    pub email: Option<&'a str>,
    pub email_verified: bool,
    pub name: Option<&'a str>,
    pub admin: bool,
    pub groups: &'a [String],
}

/// Who a dashboard session is for.
#[derive(Debug, Clone)]
pub enum SessionOwner {
    User(User),
    Token { name: String, principal: Principal },
}

/// Who a new session is for.
pub enum SessionStart<'a> {
    User(&'a str),
    /// An API token's value.
    Token(&'a str),
}

const USER_COLUMNS: &str = "u.id, u.email, u.name, u.admin, u.groups, u.created_at, u.last_login_at";

fn user(r: &tokio_postgres::Row, at: usize) -> User {
    User {
        id: r.get(at),
        email: r.get(at + 1),
        name: r.get(at + 2),
        admin: r.get(at + 3),
        groups: r.get(at + 4),
        created_on: rfc3339(r.get(at + 5)),
        last_login_on: rfc3339(r.get(at + 6)),
    }
}

fn opt_user(r: &tokio_postgres::Row, at: usize) -> Option<User> {
    r.get::<_, Option<String>>(at).map(|_| user(r, at))
}

impl Db {
    // ---- users ----

    /// Records a login: creates the user on the first one, refreshes what the
    /// provider says about them, and, if it vouches for the email, turns
    /// invitations to that email into memberships.
    pub async fn login_user(&self, l: &Login<'_>) -> Result<User> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        let row = tx
            .query_one(
                &format!(
                    "INSERT INTO users AS u (id, issuer, subject, email, name, admin, groups)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (issuer, subject) DO UPDATE
                       SET email = EXCLUDED.email, name = EXCLUDED.name, admin = EXCLUDED.admin,
                           groups = EXCLUDED.groups, last_login_at = now()
                     RETURNING {USER_COLUMNS}"
                ),
                &[&HexId::random().as_str(), &l.issuer, &l.subject, &l.email, &l.name, &l.admin, &l.groups],
            )
            .await?;
        let u = user(&row, 0);
        if let (Some(email), true) = (l.email, l.email_verified) {
            // An invitation to an account the user is in already just goes.
            tx.execute(
                "DELETE FROM account_members i WHERE i.user_id IS NULL AND lower(i.email) = lower($2)
                   AND EXISTS (SELECT 1 FROM account_members m
                               WHERE m.account_id = i.account_id AND m.user_id = $1)",
                &[&u.id, &email],
            )
            .await?;
            tx.execute(
                "UPDATE account_members SET user_id = $1, email = NULL
                 WHERE user_id IS NULL AND lower(email) = lower($2)",
                &[&u.id, &email],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(u)
    }

    pub async fn users(&self) -> Result<Vec<User>> {
        let rows = self
            .client()
            .await?
            .query(&format!("SELECT {USER_COLUMNS} FROM users u ORDER BY u.created_at"), &[])
            .await?;
        Ok(rows.iter().map(|r| user(r, 0)).collect())
    }

    /// Removes a user with their memberships and sessions. Their tokens stay.
    pub async fn delete_user(&self, id: &str) -> Result<bool> {
        Ok(self.client().await?.execute("DELETE FROM users WHERE id = $1", &[&id]).await? == 1)
    }

    /// The accounts a user may use: their memberships, and the accounts whose
    /// provider group they are in.
    pub async fn user_accounts(&self, user: &User) -> Result<Vec<AccountId>> {
        let rows = self
            .client()
            .await?
            .query(
                "SELECT account_id FROM account_members WHERE user_id = $1
                 UNION SELECT id FROM accounts WHERE oidc_group = ANY($2)",
                &[&user.id, &user.groups],
            )
            .await?;
        rows.iter().map(|r| Ok(r.get::<_, String>(0).parse()?)).collect()
    }

    // ---- members ----

    pub async fn members(&self, account: &AccountId) -> Result<Vec<Member>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT m.id, m.email, m.created_at, {USER_COLUMNS}
                     FROM account_members m LEFT JOIN users u ON u.id = m.user_id
                     WHERE m.account_id = $1 ORDER BY m.created_at"
                ),
                &[&account.as_str()],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| Member {
                id: r.get(0),
                email: r.get(1),
                created_on: rfc3339(r.get(2)),
                user: opt_user(r, 3),
            })
            .collect())
    }

    /// Adds a user who has logged in before. `None` if there is no such user
    /// or they are a member already.
    pub async fn add_member(&self, account: &AccountId, user: &str) -> Result<Option<String>> {
        let id = HexId::random().to_string();
        let n = self
            .client()
            .await?
            .execute(
                "INSERT INTO account_members (id, account_id, user_id)
                 SELECT $1, $2, id FROM users WHERE id = $3
                 ON CONFLICT DO NOTHING",
                &[&id, &account.as_str(), &user],
            )
            .await?;
        Ok((n == 1).then_some(id))
    }

    /// Invites an email: whoever logs in with it, verified by the provider,
    /// becomes a member. `None` if invited already, or if a member has it.
    pub async fn invite_member(&self, account: &AccountId, email: &str) -> Result<Option<String>> {
        let id = HexId::random().to_string();
        let n = self
            .client()
            .await?
            .execute(
                "INSERT INTO account_members (id, account_id, email)
                 SELECT $1, $2, $3
                 WHERE NOT EXISTS (SELECT 1 FROM account_members m JOIN users u ON u.id = m.user_id
                                   WHERE m.account_id = $2 AND lower(u.email) = lower($3))
                 ON CONFLICT DO NOTHING",
                &[&id, &account.as_str(), &email],
            )
            .await?;
        Ok((n == 1).then_some(id))
    }

    pub async fn remove_member(&self, account: &AccountId, member: &str) -> Result<bool> {
        Ok(self
            .client()
            .await?
            .execute(
                "DELETE FROM account_members WHERE account_id = $1 AND id = $2",
                &[&account.as_str(), &member],
            )
            .await?
            == 1)
    }

    pub async fn oidc_group(&self, account: &AccountId) -> Result<Option<String>> {
        let row = self
            .client()
            .await?
            .query_opt("SELECT oidc_group FROM accounts WHERE id = $1", &[&account.as_str()])
            .await?;
        Ok(row.and_then(|r| r.get(0)))
    }

    pub async fn set_oidc_group(&self, account: &AccountId, group: Option<&str>) -> Result<()> {
        self.client()
            .await?
            .execute("UPDATE accounts SET oidc_group = $2 WHERE id = $1", &[&account.as_str(), &group])
            .await?;
        Ok(())
    }

    // ---- tokens ----

    pub async fn tokens(&self, account: &AccountId) -> Result<Vec<TokenInfo>> {
        let rows = self
            .client()
            .await?
            .query(
                &format!(
                    "SELECT t.id, t.name, t.created_at, {USER_COLUMNS},
                       u.admin OR a.oidc_group = ANY(u.groups)
                         OR EXISTS (SELECT 1 FROM account_members m
                                    WHERE m.account_id = t.account_id AND m.user_id = u.id)
                     FROM api_tokens t
                       JOIN accounts a ON a.id = t.account_id
                       LEFT JOIN users u ON u.id = t.created_by
                     WHERE t.account_id = $1 ORDER BY t.created_at"
                ),
                &[&account.as_str()],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let created_by = opt_user(r, 3);
                TokenInfo {
                    id: r.get(0),
                    name: r.get(1),
                    created_on: rfc3339(r.get(2)),
                    creator_is_member: created_by
                        .as_ref()
                        .map(|_| r.get::<_, Option<bool>>(10) == Some(true)),
                    created_by,
                }
            })
            .collect())
    }

    /// Revokes an account token. Its dashboard sessions end with it.
    pub async fn delete_token(&self, account: &AccountId, id: &str) -> Result<bool> {
        Ok(self
            .client()
            .await?
            .execute("DELETE FROM api_tokens WHERE account_id = $1 AND id = $2", &[&account.as_str(), &id])
            .await?
            == 1)
    }

    // ---- sessions ----

    /// Starts a session. Returns the value for the cookie; only its hash is
    /// stored. Sessions past their end are cleared on the way.
    pub async fn create_session(&self, owner: SessionStart<'_>, max_age: Duration) -> Result<String> {
        let value = hex::encode(rand::random::<[u8; 32]>());
        let client = self.client().await?;
        client.execute("DELETE FROM dash_sessions WHERE expires_at < now()", &[]).await?;
        let (user, token) = match owner {
            SessionStart::User(id) => (Some(id), None),
            SessionStart::Token(t) => (None, Some(selvedge_core::sha256_hex(t.as_bytes()))),
        };
        client
            .execute(
                "INSERT INTO dash_sessions (id_sha256, user_id, token_sha256, expires_at)
                 VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
                &[&selvedge_core::sha256_hex(value.as_bytes()), &user, &token, &(max_age.as_secs() as f64)],
            )
            .await?;
        Ok(value)
    }

    /// Who the session behind a cookie value is for, unless it has ended or
    /// been idle longer than `idle`. Marks it used.
    pub async fn session(&self, value: &str, idle: Duration) -> Result<Option<SessionOwner>> {
        let client = self.client().await?;
        let id = selvedge_core::sha256_hex(value.as_bytes());
        let Some(row) = client
            .query_opt(
                &format!(
                    "SELECT s.last_seen_at < now() - interval '1 minute', t.name, t.account_id, {USER_COLUMNS}
                     FROM dash_sessions s
                       LEFT JOIN users u ON u.id = s.user_id
                       LEFT JOIN api_tokens t ON t.token_sha256 = s.token_sha256
                     WHERE s.id_sha256 = $1 AND s.expires_at > now()
                       AND s.last_seen_at > now() - make_interval(secs => $2)"
                ),
                &[&id, &(idle.as_secs() as f64)],
            )
            .await?
        else {
            return Ok(None);
        };
        // Writing on every request would cost a row version each time.
        if row.get::<_, bool>(0) {
            client
                .execute("UPDATE dash_sessions SET last_seen_at = now() WHERE id_sha256 = $1", &[&id])
                .await?;
        }
        Ok(Some(match opt_user(&row, 3) {
            Some(user) => SessionOwner::User(user),
            None => SessionOwner::Token {
                name: row.get(1),
                principal: match row.get::<_, Option<String>>(2) {
                    None => Principal::Admin,
                    Some(a) => Principal::Account(a.parse()?),
                },
            },
        }))
    }

    pub async fn end_session(&self, value: &str) -> Result<()> {
        self.client()
            .await?
            .execute(
                "DELETE FROM dash_sessions WHERE id_sha256 = $1",
                &[&selvedge_core::sha256_hex(value.as_bytes())],
            )
            .await?;
        Ok(())
    }

    // ---- accounts ----

    /// Deletes an account with everything recorded for it. Its R2 object data
    /// goes to the garbage collector. Durable Object storage (also D1's) is
    /// in the runners' cluster bucket, which the API does not reach.
    pub async fn delete_account(&self, account: &AccountId) -> Result<bool> {
        let mut client = self.client().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "INSERT INTO blob_garbage (path)
             SELECT p->>'path' FROM r2_objects o JOIN r2_buckets b ON b.id = o.bucket_id,
                    jsonb_array_elements(o.parts) p
             WHERE b.account_id = $1
             UNION SELECT up.path FROM r2_upload_parts up JOIN r2_uploads u ON u.id = up.upload_id
                     JOIN r2_buckets b ON b.id = u.bucket_id
                   WHERE b.account_id = $1
             ON CONFLICT DO NOTHING",
            &[&account.as_str()],
        )
        .await?;
        let n = tx.execute("DELETE FROM accounts WHERE id = $1", &[&account.as_str()]).await?;
        tx.commit().await?;
        Ok(n == 1)
    }
}
