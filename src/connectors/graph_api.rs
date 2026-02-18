use std::collections::HashSet;
use std::path::Path;
use std::time::Duration as StdDuration;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use reqwest::{Client, StatusCode, Url};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::warn;

use crate::connectors::{EmailConnector, ImportReport, SyncReport};
use crate::db::models::{Account, Email};
use crate::db::Database;
use crate::indexer::EmailIndex;

const GRAPH_SCOPE: &str = "https://graph.microsoft.com/.default";
const GRAPH_API_BASE: &str = "https://graph.microsoft.com/v1.0";
const CACHE_SKEW_SECONDS: i64 = 60;
const DEFAULT_DELTA_PAGE_SIZE: usize = 200;
const FULL_SYNC_PAGE_SIZE: usize = 250;
const MAX_RATE_LIMIT_RETRIES: usize = 5;
const TOKEN_CACHE_ENCRYPTION_KEY_ENV: &str = "ESS_TOKEN_CACHE_KEY";
const TOKEN_CACHE_KEY_BYTES: usize = 32;
const TOKEN_CACHE_NONCE_BYTES: usize = 12;
const TOKEN_CACHE_ENVELOPE_VERSION: u8 = 1;

const REDACTED_BODY_MAX_LEN: usize = 200;

/// A folder discovered at runtime via the Graph API mailFolders endpoint.
#[derive(Debug, Clone)]
struct DiscoveredFolder {
    /// Graph API folder ID (used in delta URLs and sync_state keys).
    folder_id: String,
    /// Human-readable folder name from the API (e.g. "Inbox", "Sent Items").
    display_name: String,
    /// Normalised label stored in ESS `emails.folder` column.
    ess_label: String,
}

/// Normalise a Graph API folder display name into an ESS folder label.
/// Well-known folders map to short canonical names; custom folders use
/// their lowercased display name as-is.
fn normalize_folder_label(display_name: &str) -> String {
    match display_name.trim().to_lowercase().as_str() {
        "inbox" => "inbox",
        "sent items" => "sent",
        "archive" => "archive",
        "drafts" => "drafts",
        "deleted items" => "trash",
        "junk email" => "spam",
        "outbox" => "outbox",
        "conversation history" => "conversation_history",
        other => return other.to_string(),
    }
    .to_string()
}

/// System/infrastructure folders that don't contain user mail.
const EXCLUDED_FOLDER_NAMES: &[&str] = &[
    "sync issues",
    "conflicts",
    "local failures",
    "server failures",
];

fn is_excluded_folder(display_name: &str) -> bool {
    let lower = display_name.trim().to_lowercase();
    EXCLUDED_FOLDER_NAMES
        .iter()
        .any(|&excluded| lower == excluded)
}

/// Map from well-known display name to the legacy graph_name used in delta
/// link keys (pre-dynamic-discovery). Used for one-time migration so existing
/// delta cursors are preserved and the 6 previously-synced folders don't
/// re-enumerate from scratch.
fn legacy_delta_key_name(display_name: &str) -> Option<&'static str> {
    match display_name.trim().to_lowercase().as_str() {
        "inbox" => Some("inbox"),
        "sent items" => Some("sentitems"),
        "archive" => Some("archive"),
        "drafts" => Some("drafts"),
        "deleted items" => Some("deleteditems"),
        "junk email" => Some("junkemail"),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GraphMailFolder {
    id: String,
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "parentFolderId")]
    #[allow(dead_code)]
    parent_folder_id: Option<String>,
    #[serde(rename = "childFolderCount")]
    child_folder_count: Option<i32>,
    #[serde(rename = "totalItemCount")]
    #[allow(dead_code)]
    total_item_count: Option<i32>,
    #[serde(rename = "isHidden")]
    #[allow(dead_code)]
    is_hidden: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphMailFolderPage {
    value: Vec<GraphMailFolder>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

const MESSAGE_SELECT_FIELDS: &str = concat!(
    "id,subject,from,toRecipients,ccRecipients,bccRecipients,receivedDateTime,sentDateTime,",
    "body,bodyPreview,importance,isRead,hasAttachments,conversationId,internetMessageId,",
    "categories,flag,webLink"
);

#[derive(Debug, Clone)]
pub struct GraphApiConnector {
    client: Client,
}

impl Default for GraphApiConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphApiConnector {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    async fn get_access_token(&self, db: &Database, account: &Account) -> Result<String> {
        if let Some(cached) = self.cached_token(db, account)? {
            return Ok(cached.access_token);
        }

        let credentials = GraphCredentials::resolve(account)?;
        let fresh = self.fetch_token(&credentials).await?;
        self.store_token(db, account, &fresh)?;
        Ok(fresh.access_token)
    }

    fn token_cache_key(account: &Account) -> String {
        format!("graph_api_token:{}", account.account_id)
    }

    fn delta_link_key(account: &Account, folder_id: &str) -> String {
        format!("graph_delta_link:{}:{}", account.account_id, folder_id)
    }

    /// Legacy key format using well-known graph_name (pre-dynamic-discovery).
    fn legacy_wellknown_delta_link_key(account: &Account, graph_name: &str) -> String {
        format!("graph_delta_link:{}:{}", account.account_id, graph_name)
    }

    /// Legacy key format (pre-multi-folder). Used for one-time migration of
    /// existing inbox delta links so users don't re-sync their entire inbox.
    fn legacy_delta_link_key(account: &Account) -> String {
        format!("graph_delta_link:{}", account.account_id)
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
            // Security default: if encryption is not configured, do not keep token data at rest.
            Self::clear_sync_state(db, &cache_key)?;
            return Ok(None);
        };

        let cached = match decrypt_cached_access_token(&raw, &encryption_key) {
            Ok(token) => token,
            Err(decrypt_error) => {
                if let Ok(legacy_token) = serde_json::from_str::<CachedAccessToken>(&raw) {
                    // One-time migration for pre-encryption plaintext cache entries.
                    self.store_token(db, account, &legacy_token)?;
                    legacy_token
                } else {
                    warn!(
                        "discarding unreadable graph token cache for account {}: {}",
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
            .context("encrypt cached graph token")?;
        db.set_sync_state(&key, &value)
            .context("write graph token to sync_state")
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

    fn load_delta_link(
        &self,
        db: &Database,
        account: &Account,
        folder: &DiscoveredFolder,
    ) -> Result<Option<String>> {
        let key = Self::delta_link_key(account, &folder.folder_id);
        let value = db
            .get_sync_state(&key)?
            .and_then(|state| state.value)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        if value.is_some() {
            return Ok(value);
        }

        // Migration layer 1: well-known-name key → folder-ID key.
        // Before dynamic discovery, delta keys used the well-known graph_name
        // (e.g. "inbox", "sentitems"). Migrate those to the new folder-ID key.
        if let Some(legacy_name) = legacy_delta_key_name(&folder.display_name) {
            let legacy_wk_key = Self::legacy_wellknown_delta_link_key(account, legacy_name);
            if let Some(legacy_value) = db
                .get_sync_state(&legacy_wk_key)?
                .and_then(|state| state.value)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
            {
                db.set_sync_state(&key, &legacy_value)
                    .context("migrate well-known delta link to folder-ID key")?;
                Self::clear_sync_state(db, &legacy_wk_key)?;
                return Ok(Some(legacy_value));
            }
        }

        // Migration layer 2: un-scoped inbox key → folder-ID key.
        // The very first version of Graph sync used a single key with no folder
        // suffix. This only applies to the inbox folder.
        if folder.display_name.trim().eq_ignore_ascii_case("inbox") {
            let legacy_key = Self::legacy_delta_link_key(account);
            if let Some(legacy_value) = db
                .get_sync_state(&legacy_key)?
                .and_then(|state| state.value)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
            {
                db.set_sync_state(&key, &legacy_value)
                    .context("migrate legacy inbox delta link")?;
                Self::clear_sync_state(db, &legacy_key)?;
                return Ok(Some(legacy_value));
            }
        }

        Ok(None)
    }

    fn store_delta_link(
        &self,
        db: &Database,
        account: &Account,
        folder: &DiscoveredFolder,
        delta_link: &str,
    ) -> Result<()> {
        let key = Self::delta_link_key(account, &folder.folder_id);
        db.set_sync_state(&key, delta_link)
            .context("persist graph delta link")
    }

    fn initial_delta_url(&self, account: &Account, folder: &DiscoveredFolder) -> Result<String> {
        let base = std::env::var("ESS_GRAPH_API_BASE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| GRAPH_API_BASE.to_string());

        let endpoint = format!(
            "{base}/users/{}/mailFolders/{}/messages/delta",
            account.email_address, folder.folder_id
        );
        let mut url =
            Url::parse(&endpoint).with_context(|| format!("parse graph URL {endpoint}"))?;
        url.query_pairs_mut()
            .append_pair("$top", &DEFAULT_DELTA_PAGE_SIZE.to_string())
            .append_pair("$select", MESSAGE_SELECT_FIELDS);
        Ok(url.to_string())
    }

    async fn fetch_token(&self, credentials: &GraphCredentials) -> Result<CachedAccessToken> {
        let token_url = std::env::var("ESS_GRAPH_TOKEN_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
                    credentials.tenant_id
                )
            });

        let response = self
            .client
            .post(&token_url)
            .form(&[
                ("client_id", credentials.client_id.as_str()),
                ("client_secret", credentials.client_secret.as_str()),
                ("scope", GRAPH_SCOPE),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await
            .with_context(|| format!("request graph oauth token from {token_url}"))?;

        let status = response.status();
        let body = response.text().await.context("read graph token response")?;
        if !status.is_success() {
            return Err(anyhow!(
                "graph oauth token request failed: status={} body={}",
                status,
                redact_response_body(&body)
            ));
        }

        let payload: OAuthTokenResponse =
            serde_json::from_str(&body).context("decode graph token JSON response")?;
        let expires_at = Utc::now()
            + Duration::seconds((payload.expires_in as i64).saturating_sub(CACHE_SKEW_SECONDS));

        Ok(CachedAccessToken {
            access_token: payload.access_token,
            expires_at,
        })
    }

    async fn fetch_delta_page_with_retry(&self, token: &str, url: &str) -> Result<GraphDeltaPage> {
        let mut backoff_seconds = 1u64;

        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .client
                .get(url)
                .bearer_auth(token)
                .header("accept", "application/json")
                .send()
                .await
                .context("request graph delta page")?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                if attempt == MAX_RATE_LIMIT_RETRIES {
                    let body = response
                        .text()
                        .await
                        .context("read graph 429 response body")?;
                    return Err(anyhow!(
                        "graph delta request exhausted retries: {}",
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
                .context("read graph delta response body")?;
            if !status.is_success() {
                return Err(anyhow!(
                    "graph delta request failed: status={} body={}",
                    status,
                    redact_response_body(&body)
                ));
            }

            let page: GraphDeltaPage =
                serde_json::from_str(&body).context("decode graph delta page JSON")?;
            return Ok(page);
        }

        Err(anyhow!("graph delta request failed without response"))
    }

    async fn fetch_folder_page_with_retry(
        &self,
        token: &str,
        url: &str,
    ) -> Result<GraphMailFolderPage> {
        let mut backoff_seconds = 1u64;

        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .client
                .get(url)
                .bearer_auth(token)
                .header("accept", "application/json")
                .send()
                .await
                .context("request graph mailFolders page")?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                if attempt == MAX_RATE_LIMIT_RETRIES {
                    let body = response
                        .text()
                        .await
                        .context("read graph 429 response body")?;
                    return Err(anyhow!(
                        "graph mailFolders request exhausted retries: {}",
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
                .context("read graph mailFolders response body")?;
            if !status.is_success() {
                return Err(anyhow!(
                    "graph mailFolders request failed: status={} body={}",
                    status,
                    redact_response_body(&body)
                ));
            }

            let page: GraphMailFolderPage =
                serde_json::from_str(&body).context("decode graph mailFolders page JSON")?;
            return Ok(page);
        }

        Err(anyhow!("graph mailFolders request failed without response"))
    }

    async fn discover_folders(
        &self,
        db: &Database,
        account: &Account,
    ) -> Result<Vec<DiscoveredFolder>> {
        let token = self.get_access_token(db, account).await?;
        let base = std::env::var("ESS_GRAPH_API_BASE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| GRAPH_API_BASE.to_string());

        let mut folders = Vec::new();
        let mut pending_parents: Vec<(String, String)> = Vec::new(); // (folder_id, display_name)

        // Fetch top-level folders
        let mut url = format!(
            "{base}/users/{}/mailFolders?includeHiddenFolders=true&$top=100",
            account.email_address
        );

        loop {
            let page = self.fetch_folder_page_with_retry(&token, &url).await?;
            for folder in &page.value {
                if is_excluded_folder(&folder.display_name) {
                    continue;
                }
                // Skip search folders (virtual folders that would create duplicates)
                if folder.display_name.eq_ignore_ascii_case("searchfolders") {
                    continue;
                }

                let ess_label = normalize_folder_label(&folder.display_name);
                folders.push(DiscoveredFolder {
                    folder_id: folder.id.clone(),
                    display_name: folder.display_name.clone(),
                    ess_label,
                });

                if folder.child_folder_count.unwrap_or(0) > 0 {
                    pending_parents.push((folder.id.clone(), folder.display_name.clone()));
                }
            }

            match page.next_link {
                Some(next) => url = next,
                None => break,
            }
        }

        // Recursively fetch child folders
        while let Some((parent_id, parent_name)) = pending_parents.pop() {
            let mut child_url = format!(
                "{base}/users/{}/mailFolders/{}/childFolders?includeHiddenFolders=true&$top=100",
                account.email_address, parent_id
            );

            loop {
                let page = self
                    .fetch_folder_page_with_retry(&token, &child_url)
                    .await?;
                for child in &page.value {
                    if is_excluded_folder(&child.display_name) {
                        continue;
                    }

                    let ess_label = format!(
                        "{}/{}",
                        normalize_folder_label(&parent_name),
                        child.display_name.trim().to_lowercase()
                    );
                    folders.push(DiscoveredFolder {
                        folder_id: child.id.clone(),
                        display_name: format!("{}/{}", parent_name, child.display_name),
                        ess_label,
                    });

                    if child.child_folder_count.unwrap_or(0) > 0 {
                        pending_parents.push((
                            child.id.clone(),
                            format!("{}/{}", parent_name, child.display_name),
                        ));
                    }
                }

                match page.next_link {
                    Some(next) => child_url = next,
                    None => break,
                }
            }
        }

        eprintln!(
            "graph: discovered {} folders for {}",
            folders.len(),
            account.account_id
        );
        for f in &folders {
            eprintln!("  {} → label={}", f.display_name, f.ess_label);
        }

        Ok(folders)
    }

    async fn fetch_messages_page_with_retry(
        &self,
        token: &str,
        url: &str,
    ) -> Result<GraphMessagesPage> {
        let mut backoff_seconds = 1u64;

        for attempt in 0..=MAX_RATE_LIMIT_RETRIES {
            let response = self
                .client
                .get(url)
                .bearer_auth(token)
                .header("accept", "application/json")
                .send()
                .await
                .context("request graph messages page")?;

            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                if attempt == MAX_RATE_LIMIT_RETRIES {
                    let body = response
                        .text()
                        .await
                        .context("read graph 429 response body")?;
                    return Err(anyhow!(
                        "graph messages request exhausted retries: {}",
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
                .context("read graph messages response body")?;
            if !status.is_success() {
                return Err(anyhow!(
                    "graph messages request failed: status={} body={}",
                    status,
                    redact_response_body(&body)
                ));
            }

            let page: GraphMessagesPage =
                serde_json::from_str(&body).context("decode graph messages page JSON")?;
            return Ok(page);
        }

        Err(anyhow!("graph messages request failed without response"))
    }

    /// Full enumeration of all messages in a folder via the plain /messages
    /// endpoint. Used for initial sync because the delta endpoint has a known
    /// Microsoft bug that caps initial results.
    async fn full_enumerate_folder(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
        folder: &DiscoveredFolder,
    ) -> Result<SyncReport> {
        let mut report = SyncReport::default();

        let base = std::env::var("ESS_GRAPH_API_BASE")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| GRAPH_API_BASE.to_string());

        let endpoint = format!(
            "{base}/users/{}/mailFolders/{}/messages",
            account.email_address, folder.folder_id
        );
        let mut url =
            Url::parse(&endpoint).with_context(|| format!("parse graph URL {endpoint}"))?;
        url.query_pairs_mut()
            .append_pair("$top", &FULL_SYNC_PAGE_SIZE.to_string())
            .append_pair("$select", MESSAGE_SELECT_FIELDS)
            .append_pair("$orderby", "receivedDateTime desc");
        let mut next_url = url.to_string();
        let mut page_number = 0u64;

        let mut consecutive_errors = 0u32;
        const MAX_CONSECUTIVE_PAGE_ERRORS: u32 = 3;

        loop {
            let token = self.get_access_token(db, account).await?;
            let page = match self
                .fetch_messages_page_with_retry(&token, &next_url)
                .await
            {
                Ok(page) => {
                    consecutive_errors = 0;
                    page
                }
                Err(error) => {
                    consecutive_errors += 1;
                    report.errors.push(format!(
                        "folder={} page_fetch_error: {error}",
                        folder.ess_label
                    ));
                    eprintln!(
                        "graph full-sync {} folder={}: page fetch error ({}/{}): {error}",
                        account.account_id,
                        folder.ess_label,
                        consecutive_errors,
                        MAX_CONSECUTIVE_PAGE_ERRORS,
                    );
                    if consecutive_errors >= MAX_CONSECUTIVE_PAGE_ERRORS {
                        eprintln!(
                            "graph full-sync {} folder={}: aborting after {} consecutive page errors",
                            account.account_id,
                            folder.ess_label,
                            consecutive_errors,
                        );
                        break;
                    }
                    // The nextLink URL is opaque — we can't skip a page. If
                    // we fail to parse the current page we have no nextLink
                    // to advance to, so we must stop.
                    break;
                }
            };

            page_number += 1;
            let page_size = page.value.len();

            for message in &page.value {
                match self.apply_message_buffered(db, indexer, account, folder, message) {
                    Ok(ApplyResult::Added) => report.emails_added += 1,
                    Ok(ApplyResult::Updated | ApplyResult::Deleted) => report.emails_updated += 1,
                    Err(error) => {
                        let message_id = message.id.as_deref().unwrap_or("<missing-id>");
                        report.errors.push(format!(
                            "folder={} id={message_id}: {error}",
                            folder.ess_label
                        ));
                    }
                }
            }

            indexer
                .commit()
                .with_context(|| format!("commit index after page {page_number}"))?;

            eprintln!(
                "graph full-sync {} folder={} ({}): page {} ({} messages), added={} updated={} errors={}",
                account.account_id,
                folder.ess_label,
                folder.display_name,
                page_number,
                page_size,
                report.emails_added,
                report.emails_updated,
                report.errors.len(),
            );

            match page.next_link {
                Some(url) => next_url = url,
                None => break,
            }
        }

        // After full enumeration, obtain a delta baseline token for future
        // incremental syncs. This re-enumerates messages (treated as upserts)
        // but the goal is to capture the deltaLink.
        eprintln!(
            "graph full-sync {} folder={}: obtaining delta baseline",
            account.account_id, folder.ess_label
        );
        let delta_url = self.initial_delta_url(account, folder)?;
        let mut next_delta_url = delta_url;
        let mut newest_delta_link: Option<String> = None;

        loop {
            let token = self.get_access_token(db, account).await?;
            let page = self
                .fetch_delta_page_with_retry(&token, &next_delta_url)
                .await?;

            // Process messages as upserts (mostly no-ops since we just enumerated)
            for message in &page.value {
                let _ = self.apply_message_buffered(db, indexer, account, folder, message);
            }
            indexer.commit().context("commit index during delta baseline")?;

            if let Some(delta_link) = page.delta_link {
                newest_delta_link = Some(delta_link);
            }

            match page.next_link {
                Some(url) => next_delta_url = url,
                None => break,
            }
        }

        if let Some(delta_link) = newest_delta_link {
            self.store_delta_link(db, account, folder, &delta_link)?;
            eprintln!(
                "graph full-sync {} folder={}: delta baseline saved",
                account.account_id, folder.ess_label
            );
        }

        Ok(report)
    }

    fn apply_message_buffered(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
        folder: &DiscoveredFolder,
        message: &GraphMessage,
    ) -> Result<ApplyResult> {
        if message.removed.is_some() {
            let id = message
                .id
                .as_deref()
                .ok_or_else(|| anyhow!("received @removed message without id"))?;
            db.conn()
                .execute("DELETE FROM emails WHERE id = ?", [id])
                .with_context(|| format!("delete removed email record {id}"))?;
            indexer
                .delete_email(id)
                .with_context(|| format!("delete removed email from index {id}"))?;
            return Ok(ApplyResult::Deleted);
        }

        let email = map_graph_message_to_email(message, account, folder)?;
        let existed = db
            .get_email(&email.id)
            .with_context(|| format!("check existing email {}", email.id))?
            .is_some();

        db.insert_email(&email)
            .with_context(|| format!("upsert graph email {}", email.id))?;
        indexer
            .add_email_buffered(&email, &account.account_type.to_string())
            .with_context(|| format!("index graph email {}", email.id))?;
        update_contact_stats(db, &email)?;

        if existed {
            Ok(ApplyResult::Updated)
        } else {
            Ok(ApplyResult::Added)
        }
    }

    async fn sync_folder(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
        folder: &DiscoveredFolder,
    ) -> Result<SyncReport> {
        // If no delta link exists, this is an initial sync — use full
        // enumeration via the /messages endpoint (the delta endpoint has a
        // known Microsoft bug that caps initial results).
        let existing_delta_link = self.load_delta_link(db, account, folder)?;
        if existing_delta_link.is_none() {
            return self
                .full_enumerate_folder(db, indexer, account, folder)
                .await;
        }

        let mut report = SyncReport::default();

        let mut next_url = existing_delta_link.unwrap();
        let mut newest_delta_link: Option<String> = None;
        let mut page_number = 0u64;

        loop {
            // Refresh token per page to avoid expiry during long syncs
            let token = self.get_access_token(db, account).await?;

            let page = self.fetch_delta_page_with_retry(&token, &next_url).await?;
            page_number += 1;
            let page_size = page.value.len();

            for message in &page.value {
                match self.apply_message_buffered(db, indexer, account, folder, message) {
                    Ok(ApplyResult::Added) => report.emails_added += 1,
                    Ok(ApplyResult::Updated | ApplyResult::Deleted) => report.emails_updated += 1,
                    Err(error) => {
                        let message_id = message.id.as_deref().unwrap_or("<missing-id>");
                        let removed_reason = message
                            .removed
                            .as_ref()
                            .and_then(|removed| removed.reason.as_deref())
                            .unwrap_or("-");
                        report.errors.push(format!(
                            "folder={} id={message_id} removed_reason={removed_reason}: {error}",
                            folder.ess_label
                        ));
                    }
                }
            }

            // Commit the index once per page (not per message)
            indexer
                .commit()
                .with_context(|| format!("commit index after page {page_number}"))?;

            eprintln!(
                "graph sync {} folder={} ({}): page {} ({} messages), added={} updated={} errors={}",
                account.account_id,
                folder.ess_label,
                folder.display_name,
                page_number,
                page_size,
                report.emails_added,
                report.emails_updated,
                report.errors.len(),
            );

            if let Some(delta_link) = page.delta_link {
                newest_delta_link = Some(delta_link);
            }

            if let Some(url) = page.next_link {
                next_url = url;
                continue;
            }
            break;
        }

        if let Some(delta_link) = newest_delta_link {
            self.store_delta_link(db, account, folder, &delta_link)?;
        }

        Ok(report)
    }
}

#[derive(Debug, Clone)]
struct GraphCredentials {
    tenant_id: String,
    client_id: String,
    client_secret: String,
}

impl GraphCredentials {
    fn resolve(account: &Account) -> Result<Self> {
        let tenant_id = std::env::var("ESS_TENANT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| account.tenant_id.clone())
            .or_else(|| config_string(account, "tenant_id"))
            .ok_or_else(|| anyhow!("missing graph tenant id (ESS_TENANT_ID/account.tenant_id)"))?;

        let client_id = std::env::var("ESS_CLIENT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| config_string(account, "client_id"))
            .ok_or_else(|| anyhow!("missing graph client id (ESS_CLIENT_ID/account.config)"))?;

        let client_secret = std::env::var("ESS_CLIENT_SECRET")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| config_string(account, "client_secret"))
            .ok_or_else(|| {
                anyhow!("missing graph client secret (ESS_CLIENT_SECRET/account.config)")
            })?;

        Ok(Self {
            tenant_id,
            client_id,
            client_secret,
        })
    }
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

fn map_graph_message_to_email(
    message: &GraphMessage,
    account: &Account,
    folder: &DiscoveredFolder,
) -> Result<Email> {
    let id = message
        .id
        .clone()
        .ok_or_else(|| anyhow!("graph message missing id"))?;

    let (from_name, from_address) = message
        .from
        .as_ref()
        .and_then(GraphRecipient::name_address_pair)
        .unwrap_or((None, None));

    let to_addresses = message
        .to_recipients
        .as_deref()
        .map(recipient_addresses)
        .unwrap_or_default();
    let cc_addresses = message
        .cc_recipients
        .as_deref()
        .map(recipient_addresses)
        .unwrap_or_default();
    let bcc_addresses = message
        .bcc_recipients
        .as_deref()
        .map(recipient_addresses)
        .unwrap_or_default();

    let (body_text, body_html) = body_fields(message.body.as_ref());
    let body_preview = message.body_preview.clone().or_else(|| {
        body_text.as_ref().and_then(|text| {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.chars().take(240).collect::<String>())
            }
        })
    });

    let received_at = message
        .received_date_time
        .clone()
        .or_else(|| message.sent_date_time.clone())
        .unwrap_or_else(|| Utc::now().to_rfc3339());

    Ok(Email {
        id,
        internet_message_id: message.internet_message_id.clone(),
        conversation_id: message.conversation_id.clone(),
        account_id: Some(account.account_id.clone()),
        subject: message.subject.clone(),
        from_address,
        from_name,
        to_addresses,
        cc_addresses,
        bcc_addresses,
        body_text,
        body_html,
        body_preview,
        received_at,
        sent_at: message.sent_date_time.clone(),
        importance: message.importance.clone(),
        is_read: message.is_read,
        has_attachments: message.has_attachments,
        folder: Some(folder.ess_label.clone()),
        categories: message.categories.clone().unwrap_or_default(),
        flag_status: message
            .flag
            .as_ref()
            .and_then(|flag| flag.flag_status.clone()),
        web_link: message.web_link.clone(),
        metadata: Some(serde_json::json!({
            "connector": "graph_api",
            "source": "graph_delta_sync"
        })),
    })
}

fn body_fields(body: Option<&GraphBody>) -> (Option<String>, Option<String>) {
    let Some(body) = body else {
        return (None, None);
    };

    let content = body
        .content
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let Some(content) = content else {
        return (None, None);
    };

    if body
        .content_type
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case("html"))
    {
        let plain = std::panic::catch_unwind(|| {
            html2text::from_read(content.as_bytes(), 120)
                .lines()
                .map(str::trim_end)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string()
        })
        .unwrap_or_default();
        let body_text = if plain.is_empty() { None } else { Some(plain) };
        return (body_text, Some(content.to_string()));
    }

    (Some(content.to_string()), None)
}

fn recipient_addresses(recipients: &[GraphRecipient]) -> Vec<String> {
    recipients
        .iter()
        .filter_map(GraphRecipient::address)
        .map(str::to_string)
        .collect()
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
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    token_type: Option<String>,
    expires_in: u64,
    ext_expires_in: Option<u64>,
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
    .map_err(|_| anyhow!("encrypt graph token cache"))?;

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
        .map_err(|_| anyhow!("decrypt graph token cache"))?;

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

#[derive(Debug, Clone, Deserialize)]
struct GraphDeltaPage {
    value: Vec<GraphMessage>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

/// Response page from the plain `/messages` list endpoint (no deltaLink).
#[derive(Debug, Clone, Deserialize)]
struct GraphMessagesPage {
    value: Vec<GraphMessage>,
    #[serde(rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphMessage {
    id: Option<String>,
    subject: Option<String>,
    from: Option<GraphRecipient>,
    #[serde(rename = "toRecipients")]
    to_recipients: Option<Vec<GraphRecipient>>,
    #[serde(rename = "ccRecipients")]
    cc_recipients: Option<Vec<GraphRecipient>>,
    #[serde(rename = "bccRecipients")]
    bcc_recipients: Option<Vec<GraphRecipient>>,
    body: Option<GraphBody>,
    #[serde(rename = "bodyPreview")]
    body_preview: Option<String>,
    importance: Option<String>,
    #[serde(rename = "isRead")]
    is_read: Option<bool>,
    #[serde(rename = "hasAttachments")]
    has_attachments: Option<bool>,
    #[serde(rename = "conversationId")]
    conversation_id: Option<String>,
    #[serde(rename = "internetMessageId")]
    internet_message_id: Option<String>,
    categories: Option<Vec<String>>,
    flag: Option<GraphFlag>,
    #[serde(rename = "webLink")]
    web_link: Option<String>,
    #[serde(rename = "receivedDateTime")]
    received_date_time: Option<String>,
    #[serde(rename = "sentDateTime")]
    sent_date_time: Option<String>,
    #[serde(rename = "@removed")]
    removed: Option<GraphRemoved>,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphRecipient {
    #[serde(rename = "emailAddress")]
    email_address: Option<GraphEmailAddress>,
}

impl GraphRecipient {
    fn address(&self) -> Option<&str> {
        self.email_address
            .as_ref()
            .and_then(|email| email.address.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn name_address_pair(&self) -> Option<(Option<String>, Option<String>)> {
        let email = self.email_address.as_ref()?;
        let name = email
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let address = email
            .address
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        Some((name, address))
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GraphEmailAddress {
    name: Option<String>,
    address: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphBody {
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphFlag {
    #[serde(rename = "flagStatus")]
    flag_status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GraphRemoved {
    reason: Option<String>,
}

#[async_trait(?Send)]
impl EmailConnector for GraphApiConnector {
    fn name(&self) -> &str {
        "graph_api"
    }

    async fn sync(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        account: &Account,
    ) -> Result<SyncReport> {
        let mut report = SyncReport::default();

        db.insert_account(account)
            .context("upsert account before graph sync")?;

        let folders = self.discover_folders(db, account).await?;

        for folder in &folders {
            eprintln!(
                "graph sync {} starting folder={} ({})",
                account.account_id, folder.ess_label, folder.display_name
            );

            match self.sync_folder(db, indexer, account, folder).await {
                Ok(folder_report) => {
                    report.emails_added += folder_report.emails_added;
                    report.emails_updated += folder_report.emails_updated;
                    report.errors.extend(folder_report.errors);
                }
                Err(error) => {
                    report.errors.push(format!(
                        "folder={} ({}): {}",
                        folder.ess_label, folder.display_name, error
                    ));
                }
            }
        }

        Ok(report)
    }

    async fn import(
        &self,
        _db: &Database,
        _indexer: &mut EmailIndex,
        _path: &Path,
        _account: &Account,
    ) -> Result<ImportReport> {
        bail!("graph_api connector does not support archive import")
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        is_excluded_folder, legacy_delta_key_name, map_graph_message_to_email,
        normalize_folder_label, CachedAccessToken, DiscoveredFolder, GraphApiConnector,
        GraphCredentials, GraphMessage, OAuthTokenResponse, TOKEN_CACHE_ENCRYPTION_KEY_ENV,
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
        std::env::temp_dir().join(format!("ess-graph-token-test-{}.db", Uuid::new_v4()))
    }

    fn test_folder(display_name: &str) -> DiscoveredFolder {
        DiscoveredFolder {
            folder_id: format!("folder-id-{}", display_name.to_lowercase().replace(' ', "-")),
            display_name: display_name.to_string(),
            ess_label: normalize_folder_label(display_name),
        }
    }

    fn account() -> Account {
        Account {
            account_id: "acc-pro".to_string(),
            email_address: "owner@example.com".to_string(),
            display_name: Some("Owner".to_string()),
            tenant_id: Some("tenant-a".to_string()),
            account_type: AccountType::Professional,
            enabled: true,
            last_sync: None,
            config: Some(json!({
                "client_id": "client-a",
                "client_secret": "secret-a"
            })),
        }
    }

    #[test]
    fn oauth_token_response_deserializes() {
        let payload = r#"{"access_token":"abc","token_type":"Bearer","expires_in":3600}"#;
        let decoded: OAuthTokenResponse =
            serde_json::from_str(payload).expect("decode oauth token response");
        assert_eq!(decoded.access_token, "abc");
        assert_eq!(decoded.expires_in, 3600);
    }

    #[test]
    fn cached_token_round_trip_in_sync_state() {
        let _lock = TOKEN_ENV_LOCK.lock().expect("lock env mutation");
        let _key_guard = TokenCacheKeyGuard::set();

        let connector = GraphApiConnector::new();
        let account = account();
        let db_path = temp_db_path();
        let db = Database::open(&db_path).expect("open db");

        let token = CachedAccessToken {
            access_token: "cached-token".to_string(),
            expires_at: chrono::Utc::now() + Duration::minutes(10),
        };
        connector
            .store_token(&db, &account, &token)
            .expect("store token");

        let cache_key = GraphApiConnector::token_cache_key(&account);
        let persisted = db
            .get_sync_state(&cache_key)
            .expect("read token cache state")
            .expect("token cache state exists")
            .value
            .expect("token cache value exists");
        assert!(!persisted.contains("cached-token"));

        let loaded = connector
            .cached_token(&db, &account)
            .expect("load token")
            .expect("token exists");
        assert_eq!(loaded.access_token, "cached-token");

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn token_cache_is_not_persisted_without_encryption_key() {
        let _lock = TOKEN_ENV_LOCK.lock().expect("lock env mutation");
        std::env::remove_var(TOKEN_CACHE_ENCRYPTION_KEY_ENV);

        let connector = GraphApiConnector::new();
        let account = account();
        let db_path = temp_db_path();
        let db = Database::open(&db_path).expect("open db");

        let token = CachedAccessToken {
            access_token: "cached-token".to_string(),
            expires_at: chrono::Utc::now() + Duration::minutes(10),
        };
        connector
            .store_token(&db, &account, &token)
            .expect("store token without key");

        let cache_key = GraphApiConnector::token_cache_key(&account);
        let cached_state = db
            .get_sync_state(&cache_key)
            .expect("read token cache state");
        assert!(cached_state.is_none());

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn credential_resolution_uses_account_config() {
        let account = account();
        let resolved = GraphCredentials::resolve(&account).expect("resolve credentials");
        assert_eq!(resolved.tenant_id, "tenant-a");
        assert_eq!(resolved.client_id, "client-a");
        assert_eq!(resolved.client_secret, "secret-a");
    }

    #[test]
    fn graph_message_maps_html_body_to_text_and_html() {
        let account = account();
        let payload = json!({
            "id": "msg-1",
            "subject": "Quarterly Review",
            "from": { "emailAddress": { "name": "Alex", "address": "alex@example.com" } },
            "toRecipients": [{ "emailAddress": { "address": "team@example.com" } }],
            "ccRecipients": [{ "emailAddress": { "address": "cc@example.com" } }],
            "body": { "contentType": "html", "content": "<p>Hello <b>team</b></p>" },
            "bodyPreview": "Hello team",
            "importance": "high",
            "isRead": false,
            "hasAttachments": false,
            "conversationId": "conv-1",
            "internetMessageId": "<msg-1@example.com>",
            "categories": ["work"],
            "flag": { "flagStatus": "flagged" },
            "webLink": "https://example.test/message/1",
            "receivedDateTime": "2026-01-01T12:00:00Z",
            "sentDateTime": "2026-01-01T11:59:00Z"
        });

        let message: GraphMessage =
            serde_json::from_value(payload).expect("deserialize graph message");
        let inbox = test_folder("Inbox");
        let mapped =
            map_graph_message_to_email(&message, &account, &inbox).expect("map graph message");
        assert_eq!(mapped.id, "msg-1");
        assert_eq!(mapped.from_address.as_deref(), Some("alex@example.com"));
        assert_eq!(
            mapped.body_html.as_deref(),
            Some("<p>Hello <b>team</b></p>")
        );
        assert!(mapped
            .body_text
            .as_deref()
            .unwrap_or_default()
            .contains("Hello"));
        assert_eq!(mapped.folder.as_deref(), Some("inbox"));
    }

    #[test]
    fn initial_delta_url_is_account_and_folder_scoped() {
        let connector = GraphApiConnector::new();
        let account = account();
        let inbox = test_folder("Inbox");
        let url = connector
            .initial_delta_url(&account, &inbox)
            .expect("build initial delta url");
        assert!(url.contains("/users/owner@example.com/mailFolders/folder-id-inbox/messages/delta"));
        assert!(url.contains("%24select="));

        let sent = test_folder("Sent Items");
        let sent_url = connector
            .initial_delta_url(&account, &sent)
            .expect("build sent delta url");
        assert!(sent_url.contains("/mailFolders/folder-id-sent-items/messages/delta"));
    }

    #[test]
    fn delta_link_key_is_folder_scoped() {
        let account = account();
        let key_a = GraphApiConnector::delta_link_key(&account, "folder-id-aaa");
        let key_b = GraphApiConnector::delta_link_key(&account, "folder-id-bbb");
        assert_eq!(key_a, "graph_delta_link:acc-pro:folder-id-aaa");
        assert_eq!(key_b, "graph_delta_link:acc-pro:folder-id-bbb");
        assert_ne!(key_a, key_b);
    }

    #[test]
    fn legacy_inbox_delta_link_is_migrated() {
        let connector = GraphApiConnector::new();
        let account = account();
        let db_path = temp_db_path();
        let db = Database::open(&db_path).expect("open db");

        // Store a delta link under the legacy (un-scoped) key.
        let legacy_key = GraphApiConnector::legacy_delta_link_key(&account);
        db.set_sync_state(&legacy_key, "https://graph.microsoft.com/v1.0/delta-link-old")
            .expect("seed legacy delta link");

        let inbox = test_folder("Inbox");
        let loaded = connector
            .load_delta_link(&db, &account, &inbox)
            .expect("load delta link")
            .expect("delta link exists");
        assert_eq!(loaded, "https://graph.microsoft.com/v1.0/delta-link-old");

        // The new folder-ID-scoped key should now hold the value.
        let new_key = GraphApiConnector::delta_link_key(&account, &inbox.folder_id);
        let new_value = db
            .get_sync_state(&new_key)
            .expect("read new key")
            .expect("new key exists")
            .value
            .expect("new key has value");
        assert_eq!(new_value, "https://graph.microsoft.com/v1.0/delta-link-old");

        // The legacy key should be removed.
        assert!(db
            .get_sync_state(&legacy_key)
            .expect("read legacy key")
            .is_none());

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn legacy_wellknown_delta_link_is_migrated() {
        let connector = GraphApiConnector::new();
        let account = account();
        let db_path = temp_db_path();
        let db = Database::open(&db_path).expect("open db");

        // Store a delta link under the old well-known-name key format.
        let legacy_wk_key =
            GraphApiConnector::legacy_wellknown_delta_link_key(&account, "sentitems");
        db.set_sync_state(
            &legacy_wk_key,
            "https://graph.microsoft.com/v1.0/delta-link-sent",
        )
        .expect("seed legacy well-known delta link");

        let sent = test_folder("Sent Items");
        let loaded = connector
            .load_delta_link(&db, &account, &sent)
            .expect("load delta link")
            .expect("delta link exists");
        assert_eq!(
            loaded,
            "https://graph.microsoft.com/v1.0/delta-link-sent"
        );

        // The new folder-ID key should hold the value.
        let new_key = GraphApiConnector::delta_link_key(&account, &sent.folder_id);
        let new_value = db
            .get_sync_state(&new_key)
            .expect("read new key")
            .expect("new key exists")
            .value
            .expect("new key has value");
        assert_eq!(
            new_value,
            "https://graph.microsoft.com/v1.0/delta-link-sent"
        );

        // The legacy well-known key should be removed.
        assert!(db
            .get_sync_state(&legacy_wk_key)
            .expect("read legacy key")
            .is_none());

        let _ = std::fs::remove_file(db_path);
    }

    #[test]
    fn folder_mapping_uses_normalized_label() {
        let account = account();
        let payload = json!({
            "id": "msg-sent-1",
            "subject": "Sent message",
            "receivedDateTime": "2026-01-01T12:00:00Z"
        });
        let message: GraphMessage =
            serde_json::from_value(payload).expect("deserialize graph message");

        let sent_folder = test_folder("Sent Items");
        let mapped =
            map_graph_message_to_email(&message, &account, &sent_folder).expect("map message");
        assert_eq!(mapped.folder.as_deref(), Some("sent"));

        let trash_folder = test_folder("Deleted Items");
        let mapped_trash =
            map_graph_message_to_email(&message, &account, &trash_folder).expect("map message");
        assert_eq!(mapped_trash.folder.as_deref(), Some("trash"));
    }

    #[test]
    fn normalize_folder_label_maps_well_known_names() {
        assert_eq!(normalize_folder_label("Inbox"), "inbox");
        assert_eq!(normalize_folder_label("Sent Items"), "sent");
        assert_eq!(normalize_folder_label("Archive"), "archive");
        assert_eq!(normalize_folder_label("Drafts"), "drafts");
        assert_eq!(normalize_folder_label("Deleted Items"), "trash");
        assert_eq!(normalize_folder_label("Junk Email"), "spam");
        assert_eq!(normalize_folder_label("Outbox"), "outbox");
        assert_eq!(normalize_folder_label("Conversation History"), "conversation_history");
        // Custom folders pass through as lowercase
        assert_eq!(normalize_folder_label("My Custom Folder"), "my custom folder");
        assert_eq!(normalize_folder_label("Blocked"), "blocked");
        assert_eq!(normalize_folder_label("Later"), "later");
    }

    #[test]
    fn excluded_folders_are_filtered() {
        assert!(is_excluded_folder("Sync Issues"));
        assert!(is_excluded_folder("sync issues"));
        assert!(is_excluded_folder("SYNC ISSUES"));
        assert!(is_excluded_folder("Conflicts"));
        assert!(is_excluded_folder("Local Failures"));
        assert!(is_excluded_folder("Server Failures"));
        assert!(!is_excluded_folder("Inbox"));
        assert!(!is_excluded_folder("Archive"));
        assert!(!is_excluded_folder("Custom Folder"));
    }

    #[test]
    fn legacy_delta_key_name_maps_well_known_folders() {
        assert_eq!(legacy_delta_key_name("Inbox"), Some("inbox"));
        assert_eq!(legacy_delta_key_name("Sent Items"), Some("sentitems"));
        assert_eq!(legacy_delta_key_name("Archive"), Some("archive"));
        assert_eq!(legacy_delta_key_name("Drafts"), Some("drafts"));
        assert_eq!(legacy_delta_key_name("Deleted Items"), Some("deleteditems"));
        assert_eq!(legacy_delta_key_name("Junk Email"), Some("junkemail"));
        assert_eq!(legacy_delta_key_name("Custom Folder"), None);
        assert_eq!(legacy_delta_key_name("Outbox"), None);
    }
}
