use std::collections::HashSet;
use std::path::Path;
use std::time::Duration as StdDuration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Duration, TimeZone, Utc};
use reqwest::{Client, StatusCode};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::warn;

use crate::connectors::{EmailConnector, ImportReport, SyncReport};
use crate::db::models::{Account, Email};
use crate::db::Database;
use crate::indexer::EmailIndex;

const GMAIL_API_BASE: &str = "https://gmail.googleapis.com/gmail/v1";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const CACHE_SKEW_SECONDS: i64 = 60;
const DEFAULT_PAGE_SIZE: usize = 100;
const MAX_RATE_LIMIT_RETRIES: usize = 5;
const TOKEN_CACHE_ENCRYPTION_KEY_ENV: &str = "ESS_TOKEN_CACHE_KEY";
const TOKEN_CACHE_KEY_BYTES: usize = 32;
const TOKEN_CACHE_NONCE_BYTES: usize = 12;
const TOKEN_CACHE_ENVELOPE_VERSION: u8 = 1;
const REDACTED_BODY_MAX_LEN: usize = 200;
const BATCH_SIZE: usize = 25;
const MAX_BATCH_RETRIES: usize = 3;
const BATCH_ENDPOINT: &str = "https://www.googleapis.com/batch/gmail/v1";

const SYSTEM_LABELS: &[&str] = &[
    "INBOX",
    "SENT",
    "DRAFT",
    "DRAFTS",
    "TRASH",
    "SPAM",
    "STARRED",
    "UNREAD",
    "IMPORTANT",
    "CATEGORY_PERSONAL",
    "CATEGORY_SOCIAL",
    "CATEGORY_PROMOTIONS",
    "CATEGORY_UPDATES",
    "CATEGORY_FORUMS",
    "CHAT",
];

#[derive(Debug, Clone)]
pub struct GmailApiConnector {
    client: Client,
}

impl Default for GmailApiConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl GmailApiConnector {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    async fn get_access_token(&self, db: &Database, account: &Account) -> Result<String> {
        if let Some(cached) = self.cached_token(db, account)? {
            return Ok(cached.access_token);
        }

        let credentials = GmailCredentials::resolve(account)?;
        let fresh = self.fetch_token(&credentials).await?;
        self.store_token(db, account, &fresh)?;
        Ok(fresh.access_token)
    }

    fn token_cache_key(account: &Account) -> String {
        format!("gmail_access_token:{}", account.account_id)
    }

    fn history_id_key(account: &Account) -> String {
        format!("gmail_history_id:{}", account.account_id)
    }

    fn cached_token(&self, db: &Database, account: &Account) -> Result<Option<CachedAccessToken>> {
        let cache_key = Self::token_cache_key(account);
        let Some(state) = db.get_sync_state(&cache_key)? else {
            return Ok(None);
        };

        let Some(raw) = state.value else {
            return Ok(None);
        };

        let Some(encryption_key) = Self::token_cache_encryption_key()? else {
            Self::clear_sync_state(db, &cache_key)?;
            return Ok(None);
        };

        let cached = match decrypt_cached_access_token(&raw, &encryption_key) {
            Ok(token) => token,
            Err(decrypt_error) => {
                if let Ok(legacy_token) = serde_json::from_str::<CachedAccessToken>(&raw) {
                    self.store_token(db, account, &legacy_token)?;
                    legacy_token
                } else {
                    warn!(
                        "discarding unreadable gmail token cache for account {}: {}",
                        account.account_id, decrypt_error
                    );
                    Self::clear_sync_state(db, &cache_key)?;
                    return Ok(None);
                }
            }
        };

        if cached.is_expired() {
            Self::clear_sync_state(db, &cache_key)?;
            return Ok(None);
        }

        Ok(Some(cached))
    }

    fn store_token(
        &self,
        db: &Database,
        account: &Account,
        token: &CachedAccessToken,
    ) -> Result<()> {
        let Some(encryption_key) = Self::token_cache_encryption_key()? else {
            return Ok(());
        };

        let key = Self::token_cache_key(account);
        let value = encrypt_cached_access_token(token, &encryption_key)
            .context("encrypt cached gmail token")?;
        db.set_sync_state(&key, &value)
            .context("write gmail token to sync_state")
    }

    fn token_cache_encryption_key() -> Result<Option<[u8; TOKEN_CACHE_KEY_BYTES]>> {
        let raw = std::env::var(TOKEN_CACHE_ENCRYPTION_KEY_ENV)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        raw.map(|value| parse_token_cache_key_hex(&value))
            .transpose()
            .with_context(|| {
                format!("{TOKEN_CACHE_ENCRYPTION_KEY_ENV} must be 64 hex characters (32 bytes)")
            })
    }

    fn clear_sync_state(db: &Database, key: &str) -> Result<()> {
        db.conn()
            .execute("DELETE FROM sync_state WHERE key = ?", [key])
            .with_context(|| format!("clear sync_state key '{key}'"))?;
        Ok(())
    }

    fn load_history_id(&self, db: &Database, account: &Account) -> Result<Option<String>> {
        let key = Self::history_id_key(account);
        Ok(db
            .get_sync_state(&key)?
            .and_then(|state| state.value)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()))
    }

    fn store_history_id(&self, db: &Database, account: &Account, history_id: &str) -> Result<()> {
        let key = Self::history_id_key(account);
        db.set_sync_state(&key, history_id)
            .context("persist gmail history id")
    }

    async fn fetch_token(&self, credentials: &GmailCredentials) -> Result<CachedAccessToken> {
        let token_url = std::env::var("ESS_GMAIL_TOKEN_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| GOOGLE_TOKEN_URL.to_string());

        let response = self
            .client
            .post(&token_url)
            .form(&[
                ("client_id", credentials.client_id.as_str()),
                ("client_secret", credentials.client_secret.as_str()),
                ("refresh_token", credentials.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await
            .with_context(|| format!("request gmail oauth token from {token_url}"))?;

        let status = response.status();
        let body = response.text().await.context("read gmail token response")?;
        if !status.is_success() {
            return Err(anyhow!(
                "gmail oauth token request failed: status={} body={}",
                status,
                redact_response_body(&body)
            ));
        }

        let payload: OAuthTokenResponse =
            serde_json::from_str(&body).context("decode gmail token JSON response")?;
        let expires_at = Utc::now()
            + Duration::seconds((payload.expires_in as i64).saturating_sub(CACHE_SKEW_SECONDS));

        Ok(CachedAccessToken {
            access_token: payload.access_token,
            expires_at,
        })
    }

    async fn fetch_with_retry(&self, token: &str, url: &str) -> Result<String> {
        let mut backoff_seconds = 1u64;

        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .client
                .get(url)
                .bearer_auth(token)
                .header("accept", "application/json")
                .send()
                .await
                .with_context(|| format!("gmail api request: {url}"))?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                if attempt == MAX_RATE_LIMIT_RETRIES {
                    let body = response
                        .text()
                        .await
                        .context("read gmail 429 response body")?;
                    return Err(anyhow!(
                        "gmail api request exhausted retries: {}",
                        redact_response_body(&body)
                    ));
                }

                let retry_after_seconds = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .unwrap_or(backoff_seconds);

                sleep(StdDuration::from_secs(retry_after_seconds)).await;
                backoff_seconds = (backoff_seconds * 2).min(32);
                continue;
            }

            let status = response.status();
            let body = response
                .text()
                .await
                .context("read gmail api response body")?;
            if !status.is_success() {
                return Err(anyhow!(
                    "gmail api request failed: status={} body={}",
                    status,
                    redact_response_body(&body)
                ));
            }

            return Ok(body);
        }

        Err(anyhow!("gmail api request failed without response"))
    }

    async fn get_profile(&self, token: &str) -> Result<GmailProfile> {
        let url = format!("{GMAIL_API_BASE}/users/me/profile");
        let body = self.fetch_with_retry(token, &url).await?;
        serde_json::from_str(&body).context("decode gmail profile")
    }

    async fn list_message_ids(
        &self,
        token: &str,
        page_token: Option<&str>,
    ) -> Result<GmailMessageList> {
        let mut url = format!("{GMAIL_API_BASE}/users/me/messages?maxResults={DEFAULT_PAGE_SIZE}");
        if let Some(pt) = page_token {
            url.push_str(&format!("&pageToken={pt}"));
        }
        let body = self.fetch_with_retry(token, &url).await?;
        serde_json::from_str(&body).context("decode gmail message list")
    }

    async fn get_message(&self, token: &str, message_id: &str) -> Result<GmailMessage> {
        let url = format!("{GMAIL_API_BASE}/users/me/messages/{message_id}?format=full");
        let body = self.fetch_with_retry(token, &url).await?;
        serde_json::from_str(&body).context("decode gmail message")
    }

    async fn list_history(
        &self,
        token: &str,
        start_history_id: &str,
        page_token: Option<&str>,
    ) -> Result<GmailHistoryList> {
        let mut url = format!(
            "{GMAIL_API_BASE}/users/me/history?startHistoryId={start_history_id}&maxResults={DEFAULT_PAGE_SIZE}"
        );
        if let Some(pt) = page_token {
            url.push_str(&format!("&pageToken={pt}"));
        }
        let body = self.fetch_with_retry(token, &url).await?;
        serde_json::from_str(&body).context("decode gmail history list")
    }

    /// Enumerate every message ID in the mailbox via messages.list pagination.
    async fn enumerate_all_message_ids(
        &self,
        db: &Database,
        account: &Account,
    ) -> Result<Vec<String>> {
        let mut all_ids = Vec::new();
        let mut page_token: Option<String> = None;
        let mut page_number = 0u64;

        loop {
            let token = self.get_access_token(db, account).await?;
            let list = self.list_message_ids(&token, page_token.as_deref()).await?;
            let messages = list.messages.unwrap_or_default();
            let page_size = messages.len();
            page_number += 1;

            for stub in messages {
                all_ids.push(stub.id);
            }

            eprintln!(
                "gmail enumerate {}: page {} ({} ids), {} total so far",
                account.account_id, page_number, page_size, all_ids.len(),
            );

            page_token = list.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(all_ids)
    }

    /// Fetch multiple messages in a single HTTP request using the Gmail batch API.
    /// Returns successfully parsed messages and retryable IDs; permanent errors go to report.
    async fn batch_get_messages(
        &self,
        token: &str,
        ids: &[String],
        report: &mut SyncReport,
    ) -> BatchParseResult {
        let empty = BatchParseResult {
            messages: Vec::new(),
            retryable_ids: ids.to_vec(),
        };

        if ids.is_empty() {
            return BatchParseResult {
                messages: Vec::new(),
                retryable_ids: Vec::new(),
            };
        }

        let boundary = format!("ess_batch_{}", uuid::Uuid::new_v4().as_simple());
        let mut body = String::new();
        for id in ids {
            body.push_str(&format!("--{boundary}\r\n"));
            body.push_str("Content-Type: application/http\r\n");
            body.push_str(&format!("Content-ID: <{id}>\r\n"));
            body.push_str("\r\n");
            body.push_str(&format!(
                "GET /gmail/v1/users/me/messages/{id}?format=full\r\n"
            ));
            body.push_str("\r\n");
        }
        body.push_str(&format!("--{boundary}--\r\n"));

        let content_type = format!("multipart/mixed; boundary={boundary}");

        let mut backoff_seconds = 1u64;
        let mut last_error = String::new();

        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = match self
                .client
                .post(BATCH_ENDPOINT)
                .bearer_auth(token)
                .header("content-type", &content_type)
                .body(body.clone())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_error = format!("batch request error: {e}");
                    if attempt < MAX_RATE_LIMIT_RETRIES {
                        sleep(StdDuration::from_secs(backoff_seconds)).await;
                        backoff_seconds = (backoff_seconds * 2).min(32);
                        continue;
                    }
                    report.errors.push(last_error);
                    return empty;
                }
            };

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                let retry_after = response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(backoff_seconds);
                sleep(StdDuration::from_secs(retry_after)).await;
                backoff_seconds = (backoff_seconds * 2).min(32);
                continue;
            }

            let status = response.status();
            let response_content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let response_body = match response.text().await {
                Ok(b) => b,
                Err(e) => {
                    report
                        .errors
                        .push(format!("batch response read error: {e}"));
                    return empty;
                }
            };

            if !status.is_success() {
                report.errors.push(format!(
                    "batch request failed: status={} body={}",
                    status,
                    redact_response_body(&response_body)
                ));
                return empty;
            }

            return parse_batch_response(&response_body, &response_content_type, ids, report);
        }

        report.errors.push(last_error);
        empty
    }

    async fn sync_full(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
    ) -> Result<SyncReport> {
        let mut report = SyncReport::default();

        // 1. Capture current historyId before enumeration
        let token = self.get_access_token(db, account).await?;
        let profile = self.get_profile(&token).await?;
        let new_history_id = profile.history_id;

        // 2. Enumerate all message IDs from the API (lightweight, IDs only)
        eprintln!(
            "gmail sync {}: enumerating all message IDs...",
            account.account_id
        );
        let all_api_ids = self.enumerate_all_message_ids(db, account).await?;
        eprintln!(
            "gmail sync {}: {} message IDs found in mailbox",
            account.account_id,
            all_api_ids.len()
        );

        // 3. Diff against DB to find missing IDs
        let existing_ids = db
            .get_email_ids_for_account(&account.account_id)
            .context("load existing email IDs for diff")?;
        let missing_ids: Vec<&String> = all_api_ids
            .iter()
            .filter(|id| !existing_ids.contains(*id))
            .collect();
        eprintln!(
            "gmail sync {}: {} already in DB, {} to fetch",
            account.account_id,
            existing_ids.len(),
            missing_ids.len()
        );

        if missing_ids.is_empty() {
            self.store_history_id(db, account, &new_history_id)?;
            return Ok(report);
        }

        // 4. Batch-fetch missing messages (newest first, already in API order)
        //    Retries 429-throttled IDs with backoff (up to MAX_BATCH_RETRIES rounds)
        let total_missing = missing_ids.len();
        let mut ids_to_fetch: Vec<String> = missing_ids
            .into_iter()
            .cloned()
            .collect();

        let mut fetched_total = 0usize;
        for retry_round in 0..=MAX_BATCH_RETRIES {
            if ids_to_fetch.is_empty() {
                break;
            }

            if retry_round > 0 {
                let backoff = StdDuration::from_secs(2u64.pow(retry_round as u32));
                eprintln!(
                    "gmail sync {}: retry round {} for {} throttled IDs (backoff {:?})",
                    account.account_id,
                    retry_round,
                    ids_to_fetch.len(),
                    backoff,
                );
                sleep(backoff).await;
            }

            let chunks: Vec<Vec<String>> = ids_to_fetch
                .chunks(BATCH_SIZE)
                .map(|chunk| chunk.to_vec())
                .collect();
            let num_chunks = chunks.len();
            let mut next_round_retries = Vec::new();

            for (batch_idx, chunk) in chunks.into_iter().enumerate() {
                let token = self.get_access_token(db, account).await?;
                let batch_result = self
                    .batch_get_messages(&token, &chunk, &mut report)
                    .await;

                for message in &batch_result.messages {
                    match self.apply_message_buffered(db, indexer, account, message) {
                        Ok(ApplyResult::Added) => report.emails_added += 1,
                        Ok(ApplyResult::Updated) => report.emails_updated += 1,
                        Err(error) => {
                            report
                                .errors
                                .push(format!("id={}: {error}", message.id));
                        }
                    }
                }

                next_round_retries.extend(batch_result.retryable_ids);

                // Commit index after each batch instead of per-email
                if let Err(e) = indexer.commit() {
                    report
                        .errors
                        .push(format!("index commit batch {}: {e}", batch_idx + 1));
                }

                fetched_total += batch_result.messages.len();
                eprintln!(
                    "gmail sync {}: batch {}/{} done, {} fetched / {} total missing",
                    account.account_id,
                    batch_idx + 1,
                    num_chunks,
                    fetched_total,
                    total_missing,
                );
            }

            ids_to_fetch = next_round_retries;
        }

        if !ids_to_fetch.is_empty() {
            eprintln!(
                "gmail sync {}: {} IDs still throttled after {} retries",
                account.account_id,
                ids_to_fetch.len(),
                MAX_BATCH_RETRIES,
            );
        }

        self.store_history_id(db, account, &new_history_id)?;
        Ok(report)
    }

    async fn sync_delta(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
        start_history_id: &str,
    ) -> Result<SyncReport> {
        let mut report = SyncReport::default();
        let mut seen_message_ids = HashSet::new();

        let mut page_token: Option<String> = None;
        let newest_history_id = loop {
            let token = self.get_access_token(db, account).await?;

            let history_list = match self
                .list_history(&token, start_history_id, page_token.as_deref())
                .await
            {
                Ok(list) => list,
                Err(error) => {
                    let error_str = format!("{error}");
                    if error_str.contains("404") || error_str.contains("historyId") {
                        warn!(
                            "gmail history expired for account {}, falling back to full sync",
                            account.account_id
                        );
                        return self.sync_full(db, indexer, account).await;
                    }
                    return Err(error);
                }
            };

            let current_history_id = history_list.history_id.clone();

            self.apply_history_records(
                db,
                indexer,
                account,
                history_list.history.unwrap_or_default(),
                &mut seen_message_ids,
                &mut report,
            )
            .await;

            page_token = history_list.next_page_token;
            if page_token.is_none() {
                break current_history_id;
            }
        };

        self.store_history_id(db, account, &newest_history_id)?;
        Ok(report)
    }

    async fn apply_history_records(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
        records: Vec<GmailHistoryRecord>,
        seen_message_ids: &mut HashSet<String>,
        report: &mut SyncReport,
    ) {
        for record in records {
            let mut message_ids = Vec::new();
            if let Some(added) = &record.messages_added {
                for entry in added {
                    message_ids.push(entry.message.id.clone());
                }
            }
            if let Some(removed) = &record.messages_deleted {
                for entry in removed {
                    let id = &entry.message.id;
                    let _ = db
                        .conn()
                        .execute("DELETE FROM emails WHERE id = ?", [id.as_str()]);
                    let _ = indexer.delete_email(id);
                    report.emails_updated += 1;
                }
            }
            if let Some(label_added) = &record.labels_added {
                for entry in label_added {
                    message_ids.push(entry.message.id.clone());
                }
            }
            if let Some(label_removed) = &record.labels_removed {
                for entry in label_removed {
                    message_ids.push(entry.message.id.clone());
                }
            }

            for msg_id in message_ids {
                if !seen_message_ids.insert(msg_id.clone()) {
                    continue;
                }
                let token = match self.get_access_token(db, account).await {
                    Ok(t) => t,
                    Err(e) => {
                        report
                            .errors
                            .push(format!("token refresh for id={msg_id}: {e}"));
                        continue;
                    }
                };
                match self.get_message(&token, &msg_id).await {
                    Ok(message) => match self.apply_message(db, indexer, account, &message) {
                        Ok(ApplyResult::Added) => report.emails_added += 1,
                        Ok(ApplyResult::Updated) => report.emails_updated += 1,
                        Err(error) => {
                            report.errors.push(format!("id={msg_id}: {error}"));
                        }
                    },
                    Err(error) => {
                        if format!("{error}").contains("404") {
                            let _ = db
                                .conn()
                                .execute("DELETE FROM emails WHERE id = ?", [msg_id.as_str()]);
                            let _ = indexer.delete_email(&msg_id);
                            report.emails_updated += 1;
                        } else {
                            report.errors.push(format!("fetch id={msg_id}: {error}"));
                        }
                    }
                }
            }
        }
    }

    fn apply_message(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
        message: &GmailMessage,
    ) -> Result<ApplyResult> {
        let email = map_gmail_message_to_email(message, account)?;
        let existed = db
            .get_email(&email.id)
            .with_context(|| format!("check existing email {}", email.id))?
            .is_some();

        db.insert_email(&email)
            .with_context(|| format!("upsert gmail email {}", email.id))?;
        indexer
            .add_email(&email, &account.account_type.to_string())
            .with_context(|| format!("index gmail email {}", email.id))?;
        update_contact_stats(db, &email)?;

        if existed {
            Ok(ApplyResult::Updated)
        } else {
            Ok(ApplyResult::Added)
        }
    }

    /// Like apply_message but buffers the index write (no commit per email).
    fn apply_message_buffered(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
        message: &GmailMessage,
    ) -> Result<ApplyResult> {
        let email = map_gmail_message_to_email(message, account)?;
        let existed = db
            .get_email(&email.id)
            .with_context(|| format!("check existing email {}", email.id))?
            .is_some();

        db.insert_email(&email)
            .with_context(|| format!("upsert gmail email {}", email.id))?;
        indexer
            .add_email_buffered(&email, &account.account_type.to_string())
            .with_context(|| format!("index gmail email {}", email.id))?;
        update_contact_stats(db, &email)?;

        if existed {
            Ok(ApplyResult::Updated)
        } else {
            Ok(ApplyResult::Added)
        }
    }
}

#[derive(Debug, Clone)]
struct GmailCredentials {
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

impl GmailCredentials {
    fn resolve(account: &Account) -> Result<Self> {
        let client_id = std::env::var("ESS_GMAIL_CLIENT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| config_string(account, "client_id"))
            .ok_or_else(|| {
                anyhow!("missing gmail client id (ESS_GMAIL_CLIENT_ID/account.config)")
            })?;

        let client_secret = std::env::var("ESS_GMAIL_CLIENT_SECRET")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| config_string(account, "client_secret"))
            .ok_or_else(|| {
                anyhow!("missing gmail client secret (ESS_GMAIL_CLIENT_SECRET/account.config)")
            })?;

        let refresh_token = std::env::var("ESS_GMAIL_REFRESH_TOKEN")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| config_string(account, "refresh_token"))
            .ok_or_else(|| {
                anyhow!("missing gmail refresh token (ESS_GMAIL_REFRESH_TOKEN/account.config)")
            })?;

        Ok(Self {
            client_id,
            client_secret,
            refresh_token,
        })
    }
}

fn config_string(account: &Account, key: &str) -> Option<String> {
    account
        .config
        .as_ref()
        .and_then(|config| config.get(key))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn redact_response_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= REDACTED_BODY_MAX_LEN {
        trimmed.to_string()
    } else {
        format!(
            "{}…[truncated {} bytes]",
            &trimmed[..REDACTED_BODY_MAX_LEN],
            trimmed.len()
        )
    }
}

/// Result of parsing a batch response: successfully parsed messages and retryable IDs.
struct BatchParseResult {
    messages: Vec<GmailMessage>,
    retryable_ids: Vec<String>,
}

/// Parse a Gmail batch API multipart/mixed response into individual GmailMessages.
/// Sub-requests that return 429 are collected as retryable IDs rather than errors.
fn parse_batch_response(
    body: &str,
    content_type: &str,
    sent_ids: &[String],
    report: &mut SyncReport,
) -> BatchParseResult {
    let mut result = BatchParseResult {
        messages: Vec::new(),
        retryable_ids: Vec::new(),
    };

    // Extract boundary from content-type header
    let boundary = content_type
        .split(';')
        .filter_map(|part| {
            part.trim()
                .strip_prefix("boundary=")
                .map(|value| value.trim_matches('"').to_string())
        })
        .next();

    let Some(boundary) = boundary else {
        report
            .errors
            .push("batch response missing boundary in content-type".to_string());
        // All IDs are retryable if we can't parse the response at all
        result.retryable_ids = sent_ids.to_vec();
        return result;
    };

    let separator = format!("--{boundary}");

    // Track which IDs were successfully parsed or permanently failed
    let mut seen_ids = HashSet::new();
    let mut part_index = 0usize;

    for part in body.split(&separator) {
        let part = part.trim();
        if part.is_empty() || part.starts_with("--") {
            continue;
        }

        // Map this part to the sent ID by position
        let current_id = sent_ids.get(part_index).cloned();
        part_index += 1;

        // Normalize line endings so we only deal with \n
        let normalized = part.replace("\r\n", "\n");

        // Find HTTP status line
        let Some(http_pos) = normalized.find("HTTP/1.1 ") else {
            continue;
        };

        let status_region = &normalized[http_pos..];
        let status_line_end = status_region.find('\n').unwrap_or(status_region.len());
        let status_line = &status_region[..status_line_end];

        // Find JSON body: first '{' after the HTTP status line
        let after_status = &normalized[http_pos + status_line_end..];
        let Some(json_start) = after_status.find('{') else {
            continue;
        };
        let json_region = &after_status[json_start..];
        let json_body = match find_json_object_end(json_region) {
            Some(end) => &json_region[..end],
            None => json_region.trim(),
        };

        if json_body.is_empty() {
            continue;
        }

        // Handle rate limiting (429) — retryable
        if status_line.contains("429") {
            if let Some(id) = &current_id {
                result.retryable_ids.push(id.clone());
                seen_ids.insert(id.clone());
            }
            continue;
        }

        // Handle other non-200 — permanent error
        if !status_line.contains("200") {
            if let Some(id) = &current_id {
                seen_ids.insert(id.clone());
            }
            report.errors.push(format!(
                "batch sub-request failed: {}",
                redact_response_body(json_body)
            ));
            continue;
        }

        // Parse the successful response
        match serde_json::from_str::<GmailMessage>(json_body) {
            Ok(message) => {
                seen_ids.insert(message.id.clone());
                result.messages.push(message);
            }
            Err(e) => {
                if let Some(id) = &current_id {
                    seen_ids.insert(id.clone());
                }
                report.errors.push(format!(
                    "batch response parse error: {e} body={}",
                    redact_response_body(json_body)
                ));
            }
        }
    }

    // Any IDs we didn't account for are also retryable (response might have been truncated)
    for id in sent_ids {
        if !seen_ids.contains(id) {
            result.retryable_ids.push(id.clone());
        }
    }

    result
}

/// Find the end of a JSON object by brace-matching. Returns index past closing '}'.
fn find_json_object_end(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, ch) in s.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escape_next = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

fn map_gmail_message_to_email(message: &GmailMessage, account: &Account) -> Result<Email> {
    let id = message.id.clone();

    let subject = extract_header(&message.payload, "Subject");
    let from_raw = extract_header(&message.payload, "From");
    let (from_name, from_address) = parse_from_header(from_raw.as_deref());
    let to_raw = extract_header(&message.payload, "To");
    let to_addresses = parse_address_list(to_raw.as_deref());
    let cc_raw = extract_header(&message.payload, "Cc");
    let cc_addresses = parse_address_list(cc_raw.as_deref());
    let bcc_raw = extract_header(&message.payload, "Bcc");
    let bcc_addresses = parse_address_list(bcc_raw.as_deref());
    let internet_message_id = extract_header(&message.payload, "Message-ID")
        .or_else(|| extract_header(&message.payload, "Message-Id"));
    let importance = extract_header(&message.payload, "Importance");
    let date_header = extract_header(&message.payload, "Date");

    let (body_text, body_html) = extract_body_parts(&message.payload);

    let body_preview = message.snippet.clone().map(|s| html_entity_decode(&s));

    // received_at from internalDate (milliseconds since epoch)
    let received_at = message
        .internal_date
        .as_deref()
        .and_then(|ms_str| ms_str.parse::<i64>().ok())
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| Utc::now().to_rfc3339());

    // sent_at from Date header
    let sent_at = date_header.as_deref().and_then(|d| {
        DateTime::parse_from_rfc2822(d)
            .ok()
            .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
    });

    let label_ids = message.label_ids.as_deref().unwrap_or_default();
    let is_read = Some(!label_ids.iter().any(|l| l == "UNREAD"));
    let has_attachments = Some(payload_has_attachments(&message.payload));
    let folder = Some(map_labels_to_folder(label_ids));
    let categories = extract_user_labels(label_ids);
    let web_link = Some(format!(
        "https://mail.google.com/mail/u/0/#inbox/{}",
        message.id
    ));

    Ok(Email {
        id,
        internet_message_id,
        conversation_id: Some(message.thread_id.clone()),
        account_id: Some(account.account_id.clone()),
        subject,
        from_address,
        from_name,
        to_addresses,
        cc_addresses,
        bcc_addresses,
        body_text,
        body_html,
        body_preview,
        received_at,
        sent_at,
        importance: Some(importance.unwrap_or_else(|| "normal".to_string())),
        is_read,
        has_attachments,
        folder,
        categories,
        flag_status: if label_ids.iter().any(|l| l == "STARRED") {
            Some("flagged".to_string())
        } else {
            None
        },
        web_link,
        metadata: Some(serde_json::json!({
            "connector": "gmail_api",
            "source": "gmail_sync"
        })),
    })
}

fn extract_header(payload: &GmailPayload, name: &str) -> Option<String> {
    payload
        .headers
        .as_deref()
        .unwrap_or_default()
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.clone())
}

fn parse_from_header(raw: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(raw) = raw else {
        return (None, None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return (None, None);
    }

    // Pattern: "Display Name <email@example.com>"
    if let Some(angle_start) = raw.rfind('<') {
        if let Some(angle_end) = raw.rfind('>') {
            let address = raw[angle_start + 1..angle_end].trim().to_string();
            let name_part = raw[..angle_start].trim();
            // Strip surrounding quotes from display name
            let name = name_part.trim_matches('"').trim().to_string();
            let name = if name.is_empty() { None } else { Some(name) };
            let address = if address.is_empty() {
                None
            } else {
                Some(address)
            };
            return (name, address);
        }
    }

    // Plain email address
    if raw.contains('@') {
        return (None, Some(raw.to_string()));
    }

    (Some(raw.to_string()), None)
}

fn parse_address_list(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };

    let mut addresses = Vec::new();
    // Split on commas, but be careful of commas inside quoted display names
    let mut depth = 0u32;
    let mut current = String::new();

    for ch in raw.chars() {
        match ch {
            '"' => {
                depth = u32::from(depth == 0);
                current.push(ch);
            }
            ',' if depth == 0 => {
                if let Some(addr) = extract_email_from_entry(current.trim()) {
                    addresses.push(addr);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if let Some(addr) = extract_email_from_entry(current.trim()) {
        addresses.push(addr);
    }

    addresses
}

fn extract_email_from_entry(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }

    if let Some(start) = entry.rfind('<') {
        if let Some(end) = entry.rfind('>') {
            let addr = entry[start + 1..end].trim();
            if !addr.is_empty() {
                return Some(addr.to_string());
            }
        }
    }

    if entry.contains('@') {
        return Some(entry.to_string());
    }

    None
}

fn extract_body_parts(payload: &GmailPayload) -> (Option<String>, Option<String>) {
    let mut text_body = None;
    let mut html_body = None;
    collect_body_parts(payload, &mut text_body, &mut html_body);

    // If we only have HTML, generate text from it
    if text_body.is_none() && html_body.is_some() {
        text_body = html_body.as_ref().and_then(|html| {
            std::panic::catch_unwind(|| {
                html2text::from_read(html.as_bytes(), 120)
                    .lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string()
            })
            .ok()
        });
        if text_body.as_deref().is_some_and(|t| t.is_empty()) {
            text_body = None;
        }
    }

    (text_body, html_body)
}

fn collect_body_parts(
    payload: &GmailPayload,
    text_body: &mut Option<String>,
    html_body: &mut Option<String>,
) {
    let mime_type = payload
        .mime_type
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();

    // Leaf node with body data
    if let Some(body) = &payload.body {
        if let Some(data) = &body.data {
            if !data.is_empty() {
                if let Ok(decoded) = decode_body_data(data) {
                    if mime_type == "text/plain" && text_body.is_none() {
                        *text_body = Some(decoded);
                    } else if mime_type == "text/html" && html_body.is_none() {
                        *html_body = Some(decoded);
                    }
                }
            }
        }
    }

    // Recurse into multipart parts
    if let Some(parts) = &payload.parts {
        for part in parts {
            collect_body_parts(part, text_body, html_body);
        }
    }
}

fn decode_body_data(data: &str) -> Result<String> {
    let bytes = URL_SAFE_NO_PAD
        .decode(data)
        .context("base64url decode gmail body data")?;
    String::from_utf8(bytes).context("utf8 decode gmail body data")
}

fn payload_has_attachments(payload: &GmailPayload) -> bool {
    if let Some(filename) = &payload.filename {
        if !filename.is_empty() {
            return true;
        }
    }
    if let Some(parts) = &payload.parts {
        for part in parts {
            if payload_has_attachments(part) {
                return true;
            }
        }
    }
    false
}

fn map_labels_to_folder(label_ids: &[String]) -> String {
    if label_ids.iter().any(|l| l == "INBOX") {
        "inbox".to_string()
    } else if label_ids.iter().any(|l| l == "SENT") {
        "sent".to_string()
    } else if label_ids.iter().any(|l| l == "DRAFT" || l == "DRAFTS") {
        "drafts".to_string()
    } else if label_ids.iter().any(|l| l == "TRASH") {
        "trash".to_string()
    } else if label_ids.iter().any(|l| l == "SPAM") {
        "spam".to_string()
    } else {
        "other".to_string()
    }
}

fn extract_user_labels(label_ids: &[String]) -> Vec<String> {
    label_ids
        .iter()
        .filter(|l| !SYSTEM_LABELS.contains(&l.as_str()))
        .cloned()
        .collect()
}

fn html_entity_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
}

fn update_contact_stats(db: &Database, email: &Email) -> Result<()> {
    let mut addresses = HashSet::new();

    if let Some(from_address) = email.from_address.as_deref() {
        let normalized = from_address.trim().to_ascii_lowercase();
        if !normalized.is_empty() {
            addresses.insert(normalized);
        }
    }

    for address in email
        .to_addresses
        .iter()
        .chain(email.cc_addresses.iter())
        .chain(email.bcc_addresses.iter())
    {
        let normalized = address.trim().to_ascii_lowercase();
        if !normalized.is_empty() {
            addresses.insert(normalized);
        }
    }

    for address in addresses {
        db.update_contact_stats(&address)
            .with_context(|| format!("update contact stats for {address}"))?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyResult {
    Added,
    Updated,
}

// --- OAuth types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    token_type: Option<String>,
    expires_in: u64,
    scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedAccessToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

impl CachedAccessToken {
    fn is_expired(&self) -> bool {
        self.expires_at <= Utc::now()
    }
}

// --- Encryption (shared pattern with graph_api) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EncryptedTokenEnvelope {
    version: u8,
    nonce_hex: String,
    ciphertext_hex: String,
}

fn encrypt_cached_access_token(
    token: &CachedAccessToken,
    encryption_key: &[u8; TOKEN_CACHE_KEY_BYTES],
) -> Result<String> {
    let mut plaintext = serde_json::to_vec(token).context("serialize token payload")?;

    let unbound_key = UnboundKey::new(&AES_256_GCM, encryption_key)
        .map_err(|_| anyhow!("construct AES-256-GCM key"))?;
    let key = LessSafeKey::new(unbound_key);

    let mut nonce_bytes = [0u8; TOKEN_CACHE_NONCE_BYTES];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow!("generate random nonce for token cache encryption"))?;

    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::empty(),
        &mut plaintext,
    )
    .map_err(|_| anyhow!("encrypt gmail token cache"))?;

    let envelope = EncryptedTokenEnvelope {
        version: TOKEN_CACHE_ENVELOPE_VERSION,
        nonce_hex: hex_encode(&nonce_bytes),
        ciphertext_hex: hex_encode(&plaintext),
    };

    serde_json::to_string(&envelope).context("serialize encrypted token envelope")
}

fn decrypt_cached_access_token(
    raw: &str,
    encryption_key: &[u8; TOKEN_CACHE_KEY_BYTES],
) -> Result<CachedAccessToken> {
    let envelope: EncryptedTokenEnvelope =
        serde_json::from_str(raw).context("parse encrypted token envelope")?;

    if envelope.version != TOKEN_CACHE_ENVELOPE_VERSION {
        return Err(anyhow!(
            "unsupported token envelope version {}",
            envelope.version
        ));
    }

    let nonce_vec = hex_decode(&envelope.nonce_hex).context("decode envelope nonce")?;
    let nonce_bytes: [u8; TOKEN_CACHE_NONCE_BYTES] = nonce_vec
        .try_into()
        .map_err(|_| anyhow!("invalid nonce length in token envelope"))?;
    let mut ciphertext =
        hex_decode(&envelope.ciphertext_hex).context("decode envelope ciphertext")?;

    let unbound_key = UnboundKey::new(&AES_256_GCM, encryption_key)
        .map_err(|_| anyhow!("construct AES-256-GCM key"))?;
    let key = LessSafeKey::new(unbound_key);

    let plaintext = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut ciphertext,
        )
        .map_err(|_| anyhow!("decrypt gmail token cache"))?;

    serde_json::from_slice(plaintext).context("parse decrypted token payload")
}

fn parse_token_cache_key_hex(raw: &str) -> Result<[u8; TOKEN_CACHE_KEY_BYTES]> {
    let decoded = hex_decode(raw).context("decode token cache key hex")?;
    decoded
        .try_into()
        .map_err(|_| anyhow!("token cache key must be 32 bytes"))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn hex_decode(raw: &str) -> Result<Vec<u8>> {
    let value = raw.trim();
    if !value.len().is_multiple_of(2) {
        return Err(anyhow!("hex string length must be even"));
    }

    let mut out = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    let mut idx = 0usize;
    while idx < bytes.len() {
        let hi = decode_hex_nibble(bytes[idx]).ok_or_else(|| anyhow!("invalid hex digit"))?;
        let lo = decode_hex_nibble(bytes[idx + 1]).ok_or_else(|| anyhow!("invalid hex digit"))?;
        out.push((hi << 4) | lo);
        idx += 2;
    }
    Ok(out)
}

fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// --- Gmail API response types ---
// #[allow(dead_code)] on these structs: fields are deserialized from the API
// but not all are read directly — they exist to match the API contract.

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GmailProfile {
    #[serde(rename = "emailAddress")]
    email_address: String,
    #[serde(rename = "historyId")]
    history_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GmailMessageList {
    messages: Option<Vec<GmailMessageStub>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "resultSizeEstimate")]
    result_size_estimate: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GmailMessageStub {
    id: String,
    #[serde(rename = "threadId")]
    thread_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct GmailMessage {
    pub id: String,
    #[serde(rename = "threadId")]
    pub thread_id: String,
    #[serde(rename = "labelIds")]
    pub label_ids: Option<Vec<String>>,
    pub snippet: Option<String>,
    pub payload: GmailPayload,
    #[serde(rename = "internalDate")]
    pub internal_date: Option<String>,
    #[serde(rename = "historyId")]
    pub history_id: Option<String>,
    #[serde(rename = "sizeEstimate")]
    pub size_estimate: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GmailPayload {
    #[serde(rename = "mimeType")]
    pub mime_type: Option<String>,
    pub headers: Option<Vec<GmailHeader>>,
    pub body: Option<GmailBody>,
    pub parts: Option<Vec<GmailPayload>>,
    pub filename: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GmailHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub(crate) struct GmailBody {
    pub size: Option<u64>,
    pub data: Option<String>,
    #[serde(rename = "attachmentId")]
    pub attachment_id: Option<String>,
}

// --- History API response types ---

#[derive(Debug, Clone, Deserialize)]
struct GmailHistoryList {
    history: Option<Vec<GmailHistoryRecord>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(rename = "historyId")]
    history_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GmailHistoryRecord {
    id: String,
    #[serde(rename = "messagesAdded")]
    messages_added: Option<Vec<GmailHistoryMessageAdded>>,
    #[serde(rename = "messagesDeleted")]
    messages_deleted: Option<Vec<GmailHistoryMessageDeleted>>,
    #[serde(rename = "labelsAdded")]
    labels_added: Option<Vec<GmailHistoryLabelEvent>>,
    #[serde(rename = "labelsRemoved")]
    labels_removed: Option<Vec<GmailHistoryLabelEvent>>,
}

#[derive(Debug, Clone, Deserialize)]
struct GmailHistoryMessageAdded {
    message: GmailMessageStub,
}

#[derive(Debug, Clone, Deserialize)]
struct GmailHistoryMessageDeleted {
    message: GmailMessageStub,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct GmailHistoryLabelEvent {
    message: GmailMessageStub,
    #[serde(rename = "labelIds")]
    label_ids: Option<Vec<String>>,
}

// --- Trait implementation ---

#[async_trait(?Send)]
impl EmailConnector for GmailApiConnector {
    fn name(&self) -> &str {
        "gmail_api"
    }

    async fn sync(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
    ) -> Result<SyncReport> {
        // Validate credentials are available before starting
        let _ = self.get_access_token(db, account).await?;

        db.insert_account(account)
            .context("upsert account before gmail sync")?;

        let saved_history_id = self.load_history_id(db, account)?;

        if let Some(history_id) = saved_history_id {
            self.sync_delta(db, indexer, account, &history_id).await
        } else {
            self.sync_full(db, indexer, account).await
        }
    }

    async fn import(
        &self,
        _db: &Database,
        _indexer: &mut EmailIndex,
        _path: &Path,
        _account: &Account,
    ) -> Result<ImportReport> {
        bail!("gmail_api connector does not support archive import")
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        map_gmail_message_to_email, CachedAccessToken, GmailApiConnector, GmailCredentials,
        GmailMessage, OAuthTokenResponse, TOKEN_CACHE_ENCRYPTION_KEY_ENV,
    };
    use crate::connectors::TOKEN_ENV_LOCK;
    use crate::db::models::{Account, AccountType};
    use crate::db::Database;

    const TEST_TOKEN_CACHE_KEY_HEX: &str =
        "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    struct TokenCacheKeyGuard;

    impl TokenCacheKeyGuard {
        fn set() -> Self {
            std::env::set_var(TOKEN_CACHE_ENCRYPTION_KEY_ENV, TEST_TOKEN_CACHE_KEY_HEX);
            Self
        }
    }

    impl Drop for TokenCacheKeyGuard {
        fn drop(&mut self) {
            std::env::remove_var(TOKEN_CACHE_ENCRYPTION_KEY_ENV);
        }
    }

    fn temp_db_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ess-gmail-token-test-{}.db", Uuid::new_v4()))
    }

    fn account() -> Account {
        Account {
            account_id: "acc-gmail".to_string(),
            email_address: "user@gmail.com".to_string(),
            display_name: Some("Gmail User".to_string()),
            tenant_id: None,
            account_type: AccountType::Personal,
            enabled: true,
            last_sync: None,
            config: Some(json!({
                "connector": "gmail_api",
                "client_id": "gmail-client-id",
                "client_secret": "gmail-client-secret",
                "refresh_token": "gmail-refresh-token"
            })),
        }
    }

    #[test]
    fn gmail_oauth_token_response_deserializes() {
        let payload = r#"{"access_token":"ya29.abc","token_type":"Bearer","expires_in":3600,"scope":"https://www.googleapis.com/auth/gmail.readonly"}"#;
        let decoded: OAuthTokenResponse =
            serde_json::from_str(payload).expect("decode oauth token response");
        assert_eq!(decoded.access_token, "ya29.abc");
        assert_eq!(decoded.expires_in, 3600);
    }

    #[test]
    fn gmail_cached_token_round_trip_in_sync_state() {
        let _lock = TOKEN_ENV_LOCK.lock().expect("lock env mutation");
        let _key_guard = TokenCacheKeyGuard::set();

        let connector = GmailApiConnector::new();
        let account = account();
        let db_path = temp_db_path();
        let db = Database::open(&db_path).expect("open db");

        let token = CachedAccessToken {
            access_token: "cached-gmail-token".to_string(),
            expires_at: chrono::Utc::now() + Duration::minutes(10),
        };
        connector
            .store_token(&db, &account, &token)
            .expect("store token");

        let cache_key = GmailApiConnector::token_cache_key(&account);
        let persisted = db
            .get_sync_state(&cache_key)
            .expect("read token cache state")
            .expect("token cache state exists")
            .value
            .expect("token cache value exists");
        assert!(!persisted.contains("cached-gmail-token"));

        let loaded = connector
            .cached_token(&db, &account)
            .expect("load token")
            .expect("token exists");
        assert_eq!(loaded.access_token, "cached-gmail-token");

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn gmail_token_cache_not_persisted_without_encryption_key() {
        let _lock = TOKEN_ENV_LOCK.lock().expect("lock env mutation");
        std::env::remove_var(TOKEN_CACHE_ENCRYPTION_KEY_ENV);

        let connector = GmailApiConnector::new();
        let account = account();
        let db_path = temp_db_path();
        let db = Database::open(&db_path).expect("open db");

        let token = CachedAccessToken {
            access_token: "cached-gmail-token".to_string(),
            expires_at: chrono::Utc::now() + Duration::minutes(10),
        };
        connector
            .store_token(&db, &account, &token)
            .expect("store token without key");

        let cache_key = GmailApiConnector::token_cache_key(&account);
        let cached_state = db
            .get_sync_state(&cache_key)
            .expect("read token cache state");
        assert!(cached_state.is_none());

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn gmail_credential_resolution_uses_account_config() {
        let account = account();
        let resolved = GmailCredentials::resolve(&account).expect("resolve credentials");
        assert_eq!(resolved.client_id, "gmail-client-id");
        assert_eq!(resolved.client_secret, "gmail-client-secret");
        assert_eq!(resolved.refresh_token, "gmail-refresh-token");
    }

    #[test]
    fn gmail_message_maps_to_email() {
        let account = account();
        let payload = json!({
            "id": "18e1234abcd",
            "threadId": "18e1234abcd",
            "labelIds": ["INBOX", "UNREAD", "IMPORTANT", "Label_42"],
            "snippet": "Hello &amp; welcome to the meeting",
            "payload": {
                "mimeType": "multipart/alternative",
                "headers": [
                    { "name": "Subject", "value": "Quarterly Review" },
                    { "name": "From", "value": "Alex Smith <alex@example.com>" },
                    { "name": "To", "value": "team@example.com, Bob <bob@example.com>" },
                    { "name": "Cc", "value": "cc@example.com" },
                    { "name": "Message-ID", "value": "<msg-1@mail.gmail.com>" },
                    { "name": "Date", "value": "Wed, 01 Jan 2026 12:00:00 +0000" },
                    { "name": "Importance", "value": "high" }
                ],
                "body": { "size": 0 },
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "headers": [],
                        "body": {
                            "size": 16,
                            "data": "SGVsbG8gdGVhbSE"
                        }
                    },
                    {
                        "mimeType": "text/html",
                        "headers": [],
                        "body": {
                            "size": 30,
                            "data": "PHA-SGVsbG8gPGI-dGVhbTwvYj4hPC9wPg"
                        }
                    }
                ]
            },
            "internalDate": "1735732800000",
            "historyId": "12345",
            "sizeEstimate": 5000
        });

        let message: GmailMessage =
            serde_json::from_value(payload).expect("deserialize gmail message");
        let mapped = map_gmail_message_to_email(&message, &account).expect("map gmail message");

        assert_eq!(mapped.id, "18e1234abcd");
        assert_eq!(mapped.conversation_id.as_deref(), Some("18e1234abcd"));
        assert_eq!(
            mapped.internet_message_id.as_deref(),
            Some("<msg-1@mail.gmail.com>")
        );
        assert_eq!(mapped.subject.as_deref(), Some("Quarterly Review"));
        assert_eq!(mapped.from_name.as_deref(), Some("Alex Smith"));
        assert_eq!(mapped.from_address.as_deref(), Some("alex@example.com"));
        assert_eq!(mapped.to_addresses.len(), 2);
        assert_eq!(mapped.to_addresses[0], "team@example.com");
        assert_eq!(mapped.to_addresses[1], "bob@example.com");
        assert_eq!(mapped.cc_addresses, vec!["cc@example.com"]);
        assert_eq!(mapped.body_text.as_deref(), Some("Hello team!"));
        assert!(mapped.body_html.is_some());
        assert_eq!(
            mapped.body_preview.as_deref(),
            Some("Hello & welcome to the meeting")
        );
        assert_eq!(mapped.importance.as_deref(), Some("high"));
        assert_eq!(mapped.is_read, Some(false)); // UNREAD label present
        assert_eq!(mapped.folder.as_deref(), Some("inbox"));
        assert_eq!(mapped.categories, vec!["Label_42"]);
        assert!(mapped.web_link.as_deref().unwrap().contains("18e1234abcd"));
    }

    #[test]
    fn gmail_message_plain_text_only() {
        let account = account();
        let payload = json!({
            "id": "msg-plain",
            "threadId": "thread-plain",
            "labelIds": ["INBOX"],
            "snippet": "Just plain text",
            "payload": {
                "mimeType": "text/plain",
                "headers": [
                    { "name": "Subject", "value": "Plain email" },
                    { "name": "From", "value": "sender@example.com" }
                ],
                "body": {
                    "size": 15,
                    "data": "SnVzdCBwbGFpbiB0ZXh0"
                }
            },
            "internalDate": "1735732800000"
        });

        let message: GmailMessage =
            serde_json::from_value(payload).expect("deserialize gmail message");
        let mapped = map_gmail_message_to_email(&message, &account).expect("map gmail message");

        assert_eq!(mapped.id, "msg-plain");
        assert_eq!(mapped.body_text.as_deref(), Some("Just plain text"));
        assert!(mapped.body_html.is_none());
        assert_eq!(mapped.is_read, Some(true)); // No UNREAD label
        assert_eq!(mapped.from_address.as_deref(), Some("sender@example.com"));
        assert!(mapped.from_name.is_none());
    }

    #[test]
    fn gmail_message_with_attachment() {
        let account = account();
        let payload = json!({
            "id": "msg-attach",
            "threadId": "thread-attach",
            "labelIds": ["INBOX"],
            "snippet": "See attached",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [
                    { "name": "Subject", "value": "With attachment" },
                    { "name": "From", "value": "sender@example.com" }
                ],
                "body": { "size": 0 },
                "parts": [
                    {
                        "mimeType": "text/plain",
                        "headers": [],
                        "body": { "size": 12, "data": "U2VlIGF0dGFjaGVk" }
                    },
                    {
                        "mimeType": "application/pdf",
                        "filename": "report.pdf",
                        "headers": [],
                        "body": { "size": 50000, "attachmentId": "att-1" }
                    }
                ]
            },
            "internalDate": "1735732800000"
        });

        let message: GmailMessage =
            serde_json::from_value(payload).expect("deserialize gmail message");
        let mapped = map_gmail_message_to_email(&message, &account).expect("map gmail message");

        assert_eq!(mapped.has_attachments, Some(true));
    }

    #[test]
    fn gmail_label_to_folder_mapping() {
        use super::map_labels_to_folder;

        assert_eq!(
            map_labels_to_folder(&["INBOX".to_string(), "UNREAD".to_string()]),
            "inbox"
        );
        assert_eq!(map_labels_to_folder(&["SENT".to_string()]), "sent");
        assert_eq!(map_labels_to_folder(&["DRAFT".to_string()]), "drafts");
        assert_eq!(map_labels_to_folder(&["TRASH".to_string()]), "trash");
        assert_eq!(map_labels_to_folder(&["SPAM".to_string()]), "spam");
        assert_eq!(map_labels_to_folder(&["Label_1".to_string()]), "other");
    }

    #[test]
    fn gmail_from_header_parsing() {
        use super::parse_from_header;

        let (name, addr) = parse_from_header(Some("Alex Smith <alex@example.com>"));
        assert_eq!(name.as_deref(), Some("Alex Smith"));
        assert_eq!(addr.as_deref(), Some("alex@example.com"));

        let (name, addr) = parse_from_header(Some("\"Smith, Alex\" <alex@example.com>"));
        assert_eq!(name.as_deref(), Some("Smith, Alex"));
        assert_eq!(addr.as_deref(), Some("alex@example.com"));

        let (name, addr) = parse_from_header(Some("plain@example.com"));
        assert!(name.is_none());
        assert_eq!(addr.as_deref(), Some("plain@example.com"));

        let (name, addr) = parse_from_header(None);
        assert!(name.is_none());
        assert!(addr.is_none());
    }

    #[test]
    fn gmail_address_list_parsing() {
        use super::parse_address_list;

        let addrs = parse_address_list(Some(
            "team@example.com, \"Bob, Jr.\" <bob@example.com>, alice@example.com",
        ));
        assert_eq!(addrs.len(), 3);
        assert_eq!(addrs[0], "team@example.com");
        assert_eq!(addrs[1], "bob@example.com");
        assert_eq!(addrs[2], "alice@example.com");

        let empty = parse_address_list(None);
        assert!(empty.is_empty());
    }

    #[test]
    fn gmail_html_entity_decode() {
        use super::html_entity_decode;
        assert_eq!(
            html_entity_decode("Hello &amp; welcome &lt;team&gt;"),
            "Hello & welcome <team>"
        );
    }

    #[test]
    fn gmail_history_id_key_format() {
        let account = account();
        let key = GmailApiConnector::history_id_key(&account);
        assert_eq!(key, "gmail_history_id:acc-gmail");
    }

    #[test]
    fn gmail_base64url_decode() {
        use super::decode_body_data;
        let result = decode_body_data("SGVsbG8gV29ybGQ").expect("decode");
        assert_eq!(result, "Hello World");
    }
}
