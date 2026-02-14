use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use regex::Regex;
use serde_json::{json, Value};

use crate::connectors::{EmailConnector, ImportReport, SyncReport};
use crate::db::models::Account;
use crate::db::models::Email;
use crate::db::Database;
use crate::indexer::EmailIndex;

#[derive(Debug, Default, Clone)]
pub struct JsonArchiveConnector;

impl JsonArchiveConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait(?Send)]
impl EmailConnector for JsonArchiveConnector {
    fn name(&self) -> &str {
        "json_archive"
    }

    async fn sync(
        &self,
        _db: &Database,
        _indexer: &mut EmailIndex,
        _account: &Account,
    ) -> Result<SyncReport> {
        bail!("json_archive connector does not support live sync; use import")
    }

    async fn import(
        &self,
        db: &Database,
        indexer: &mut EmailIndex,
        path: &Path,
        account: &Account,
    ) -> Result<ImportReport> {
        db.insert_account(account)
            .context("upsert account before JSON archive import")?;

        let mut report = ImportReport::default();
        let files = collect_json_files(path)?;

        for file_path in files {
            report.files_processed += 1;

            match import_file(db, indexer, account, &file_path) {
                Ok(imported) => {
                    if imported {
                        report.emails_imported += 1;
                    }
                }
                Err(error) => {
                    report
                        .errors
                        .push(format!("{}: {error}", file_path.display()));
                }
            }
        }

        Ok(report)
    }
}

fn collect_json_files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            return Ok(vec![path.to_path_buf()]);
        }
        return Err(anyhow!("expected .json file, got {}", path.display()));
    }

    if !path.is_dir() {
        return Err(anyhow!(
            "import path does not exist or is not a file/directory: {}",
            path.display()
        ));
    }

    let mut files = Vec::new();
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("read archive directory {}", path.display()))?
    {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_file()
            && entry_path.extension().and_then(|ext| ext.to_str()) == Some("json")
        {
            files.push(entry_path);
        }
    }

    files.sort();
    Ok(files)
}

fn import_file(
    db: &Database,
    indexer: &mut EmailIndex,
    account: &Account,
    file_path: &Path,
) -> Result<bool> {
    let raw = std::fs::read_to_string(file_path)
        .with_context(|| format!("read JSON archive file {}", file_path.display()))?;
    let payload: Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse JSON archive file {}", file_path.display()))?;

    let email = map_archive_payload(&payload, account, file_path)?;
    if db.get_email(&email.id)?.is_some() {
        return Ok(false);
    }

    db.insert_email(&email)
        .with_context(|| format!("insert imported email {}", email.id))?;
    indexer
        .add_email(&email, &account.account_type.to_string())
        .with_context(|| format!("index imported email {}", email.id))?;
    update_contact_stats(db, &email)?;

    Ok(true)
}

fn map_archive_payload(payload: &Value, account: &Account, source_path: &Path) -> Result<Email> {
    let record = payload.get("email").unwrap_or(payload);

    let id = get_str(record, &["id"])
        .or_else(|| get_str(payload, &["id", "graph_id"]))
        .ok_or_else(|| anyhow!("missing id/graph_id"))?;

    let received_at = get_str(record, &["receivedDateTime"])
        .or_else(|| get_str(payload, &["receivedDateTime"]))
        .or_else(|| get_str(record, &["sentDateTime"]))
        .or_else(|| get_str(payload, &["sentDateTime"]))
        .or_else(|| get_str(payload, &["archivedAt", "archived_at"]))
        .unwrap_or_else(|| Utc::now().to_rfc3339());

    let sent_at =
        get_str(record, &["sentDateTime"]).or_else(|| get_str(payload, &["sentDateTime"]));
    let subject = get_str(record, &["subject"]).or_else(|| get_str(payload, &["subject"]));

    let headers = field(record, payload, &["headers"]);
    let from_value = field(record, payload, &["from", "sender"]);
    let (from_name, mut from_address) = parse_contact(from_value).unwrap_or((None, None));
    if from_address.is_none() {
        from_address = header_value(headers, &["From", "from"])
            .and_then(|header| parse_first_email_from_header(&header));
    }

    let mut to_addresses = parse_recipients(field(record, payload, &["toRecipients", "to"]));
    let mut cc_addresses = parse_recipients(field(record, payload, &["ccRecipients", "cc"]));
    let mut bcc_addresses = parse_recipients(field(record, payload, &["bccRecipients", "bcc"]));
    if to_addresses.is_empty() {
        if let Some(header_to) = header_value(headers, &["To", "to"]) {
            to_addresses = parse_addresses_from_header(&header_to);
        }
    }
    if cc_addresses.is_empty() {
        if let Some(header_cc) = header_value(headers, &["Cc", "CC", "cc"]) {
            cc_addresses = parse_addresses_from_header(&header_cc);
        }
    }
    if bcc_addresses.is_empty() {
        if let Some(header_bcc) = header_value(headers, &["Bcc", "BCC", "bcc"]) {
            bcc_addresses = parse_addresses_from_header(&header_bcc);
        }
    }

    let (body_text, body_html, body_preview) = parse_body(record, payload);
    let internet_message_id = header_value(headers, &["Message-ID", "messageId"])
        .or_else(|| get_str(record, &["internetMessageId"]))
        .or_else(|| get_str(payload, &["internetMessageId"]));

    let conversation_id = get_str(record, &["conversationId"])
        .or_else(|| get_str(payload, &["conversationId"]))
        .or_else(|| {
            header_value(headers, &["Thread-Topic", "threadTopic"])
                .map(|topic| format!("thread-{}", stable_hash_hex(&topic)))
        });

    let importance = get_str(record, &["importance"]).or_else(|| get_str(payload, &["importance"]));
    let is_read = get_bool(record, &["isRead"]).or_else(|| get_bool(payload, &["isRead"]));
    let has_attachments =
        get_bool(record, &["hasAttachments"]).or_else(|| get_bool(payload, &["hasAttachments"]));
    let folder =
        get_str(record, &["folder", "direction"]).or_else(|| get_str(payload, &["direction"]));

    let categories = field(record, payload, &["categories"])
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let metadata = Some(json!({
        "source_file": source_path.file_name().and_then(|name| name.to_str()).unwrap_or_default(),
        "archive_connector": "json_archive",
    }));

    Ok(Email {
        id,
        internet_message_id,
        conversation_id,
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
        importance,
        is_read,
        has_attachments,
        folder,
        categories,
        flag_status: None,
        web_link: get_str(record, &["webLink"]).or_else(|| get_str(payload, &["webLink"])),
        metadata,
    })
}

fn update_contact_stats(db: &Database, email: &Email) -> Result<()> {
    let mut unique_addresses: HashSet<String> = HashSet::new();

    if let Some(from_address) = email
        .from_address
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        unique_addresses.insert(from_address.to_ascii_lowercase());
    }

    for address in email
        .to_addresses
        .iter()
        .chain(email.cc_addresses.iter())
        .chain(email.bcc_addresses.iter())
    {
        let normalized = address.trim().to_ascii_lowercase();
        if !normalized.is_empty() {
            unique_addresses.insert(normalized);
        }
    }

    for address in unique_addresses {
        db.update_contact_stats(&address)
            .with_context(|| format!("update contact stats for {address}"))?;
    }

    Ok(())
}

fn field<'a>(record: &'a Value, payload: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter()
        .find_map(|key| record.get(*key))
        .or_else(|| keys.iter().find_map(|key| payload.get(*key)))
}

fn get_str(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn get_bool(value: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| value.get(*key))
        .and_then(Value::as_bool)
}

fn parse_contact(value: Option<&Value>) -> Option<(Option<String>, Option<String>)> {
    let value = value?;

    let direct_name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string);
    let direct_address = value
        .get("address")
        .or_else(|| value.get("email"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let email_address = value.get("emailAddress");
    let nested_name = email_address
        .and_then(|nested| nested.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let nested_address = email_address
        .and_then(|nested| nested.get("address"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if direct_name.is_none()
        && direct_address.is_none()
        && nested_name.is_none()
        && nested_address.is_none()
    {
        return None;
    }

    Some((
        direct_name.or(nested_name),
        direct_address
            .or(nested_address)
            .map(|value| value.to_ascii_lowercase()),
    ))
}

fn parse_recipients(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(values)) = value else {
        return Vec::new();
    };

    values
        .iter()
        .filter_map(|entry| parse_contact(Some(entry)).and_then(|(_, address)| address))
        .map(|address| address.to_ascii_lowercase())
        .collect()
}

fn parse_body(record: &Value, payload: &Value) -> (Option<String>, Option<String>, Option<String>) {
    let preview = get_str(record, &["bodyPreview", "body_summary"])
        .or_else(|| get_str(payload, &["bodyPreview", "body_summary"]));
    let body_value = field(record, payload, &["body"]);
    let body_content_type = get_str(record, &["bodyContentType"]).or_else(|| {
        body_value
            .and_then(|body| body.get("contentType"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    match body_value {
        Some(Value::Object(body_obj)) => {
            let content = body_obj
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string);
            let is_html = body_obj
                .get("contentType")
                .and_then(Value::as_str)
                .map(|kind| kind.eq_ignore_ascii_case("html"))
                .or_else(|| {
                    body_content_type
                        .as_ref()
                        .map(|kind| kind.eq_ignore_ascii_case("html"))
                })
                .unwrap_or(false);

            if is_html {
                let html = content.clone();
                let text = preview
                    .clone()
                    .or_else(|| content.as_ref().map(|body| html_to_text(body)));
                return (text, html, preview);
            }

            let text = content.or_else(|| preview.clone());
            (text, None, preview)
        }
        Some(Value::String(content)) => {
            let is_html = body_content_type
                .as_deref()
                .map(|kind| kind.eq_ignore_ascii_case("html"))
                .unwrap_or_else(|| looks_like_html(content));
            if is_html {
                (
                    preview.clone().or_else(|| Some(html_to_text(content))),
                    Some(content.to_string()),
                    preview,
                )
            } else {
                (Some(content.to_string()), None, preview)
            }
        }
        _ => (preview.clone(), None, preview),
    }
}

fn header_value(headers: Option<&Value>, keys: &[&str]) -> Option<String> {
    let headers_obj = headers?.as_object()?;
    for (header_name, header_value) in headers_obj {
        if keys
            .iter()
            .any(|wanted| header_name.eq_ignore_ascii_case(wanted))
        {
            if let Some(value) = header_value.as_str() {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

fn stable_hash_hex(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn html_to_text(html: &str) -> String {
    let rendered = html2text::from_read(html.as_bytes(), 120);
    rendered.trim().to_string()
}

fn looks_like_html(value: &str) -> bool {
    value.contains("<html") || value.contains("<body") || value.contains("</")
}

fn parse_first_email_from_header(value: &str) -> Option<String> {
    let email_pattern = Regex::new(r"(?i)<([^>]+@[^>]+)>").expect("compile email header regex");
    if let Some(captures) = email_pattern.captures(value) {
        return captures
            .get(1)
            .map(|capture| capture.as_str().trim().to_ascii_lowercase());
    }

    let fallback = value.trim().trim_matches('"').to_ascii_lowercase();
    if fallback.contains('@') {
        Some(fallback)
    } else {
        None
    }
}

fn parse_addresses_from_header(value: &str) -> Vec<String> {
    let email_pattern = Regex::new(r"(?i)<([^>]+@[^>]+)>").expect("compile email header regex");
    let mut addresses: Vec<String> = email_pattern
        .captures_iter(value)
        .filter_map(|captures| {
            captures
                .get(1)
                .map(|capture| capture.as_str().trim().to_ascii_lowercase())
        })
        .collect();

    if addresses.is_empty() {
        for part in value.split(',') {
            if let Some(address) = parse_first_email_from_header(part) {
                addresses.push(address);
            }
        }
    }

    addresses
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use serde_json::json;
    use uuid::Uuid;

    use crate::connectors::EmailConnector;
    use crate::db::models::{Account, AccountType};
    use crate::indexer::SearchFilters;

    use super::{map_archive_payload, JsonArchiveConnector};

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("ess-json-archive-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }

    fn sample_account() -> Account {
        Account {
            account_id: "acc-1".to_string(),
            email_address: "owner@example.com".to_string(),
            display_name: Some("Owner".to_string()),
            tenant_id: None,
            account_type: AccountType::Professional,
            enabled: true,
            last_sync: None,
            config: None,
        }
    }

    #[test]
    fn map_archive_payload_handles_thread_topic_and_message_id() {
        let account = sample_account();
        let payload = json!({
            "id": "msg-1",
            "subject": "Kickoff",
            "receivedDateTime": "2026-01-01T10:00:00Z",
            "from": { "name": "Alice", "address": "Alice@Example.com" },
            "toRecipients": [{ "name": "Bob", "address": "bob@example.com" }],
            "bodyPreview": "Kickoff preview",
            "headers": {
                "Thread-Topic": "Kickoff Thread",
                "Message-ID": "<msg-1@example.com>"
            }
        });

        let email = map_archive_payload(&payload, &account, Path::new("sample.json"))
            .expect("map archive payload");
        assert_eq!(email.id, "msg-1");
        assert_eq!(
            email.internet_message_id.as_deref(),
            Some("<msg-1@example.com>")
        );
        assert!(email
            .conversation_id
            .as_deref()
            .unwrap_or_default()
            .starts_with("thread-"));
        assert_eq!(email.from_address.as_deref(), Some("alice@example.com"));
    }

    #[tokio::test]
    async fn import_directory_dedupes_and_indexes_emails() -> Result<()> {
        let root = temp_root();
        let archive_dir = root.join("archive");
        std::fs::create_dir_all(&archive_dir)?;

        let payload = json!({
            "id": "msg-1",
            "subject": "Kickoff planning",
            "receivedDateTime": "2026-01-01T10:00:00Z",
            "from": { "name": "Alice", "address": "alice@example.com" },
            "toRecipients": [{ "name": "Bob", "address": "bob@example.com" }],
            "importance": "normal",
            "isRead": false,
            "hasAttachments": false,
            "body": { "contentType": "text", "content": "Kickoff on Friday." },
            "headers": {
                "Thread-Topic": "Kickoff Thread",
                "Message-ID": "<msg-1@example.com>"
            }
        });

        std::fs::write(
            archive_dir.join("one.json"),
            serde_json::to_string_pretty(&payload)?,
        )?;
        std::fs::write(
            archive_dir.join("duplicate.json"),
            serde_json::to_string_pretty(&payload)?,
        )?;

        let db = crate::db::Database::open(&root.join("ess.db"))?;
        let mut index = crate::indexer::EmailIndex::open(&root.join("index"))?;
        let account = sample_account();
        let connector = JsonArchiveConnector::new();

        let report = connector
            .import(&db, &mut index, &archive_dir, &account)
            .await?;
        assert_eq!(report.files_processed, 2);
        assert_eq!(report.emails_imported, 1);
        assert!(report.errors.is_empty());

        let indexed = index.search("kickoff", &SearchFilters::default(), 10)?;
        assert_eq!(indexed.len(), 1);
        assert_eq!(indexed[0].email_db_id, "msg-1");

        let _ = std::fs::remove_dir_all(root);
        Ok(())
    }
}
