use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use chrono::NaiveDate;
use serde_json::{json, Value};

use crate::db::{Database, EmailSearchFilters};
use crate::indexer::EmailIndex;
use crate::search;
use crate::search::filters::{EmailFilters, Scope};

pub fn tool_schemas() -> Vec<Value> {
    vec![
        json!({
            "name": "ess_search",
            "description": "Search indexed emails",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "from": {"type": "string"},
                    "to": {"type": "string"},
                    "since": {"type": "string"},
                    "until": {"type": "string"},
                    "scope": {"type": "string"},
                    "account": {"type": "string"},
                    "folder": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "ess_thread",
            "description": "Return messages for a conversation",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "conversation_id": {"type": "string"}
                },
                "required": ["conversation_id"]
            }
        }),
        json!({
            "name": "ess_contacts",
            "description": "Search contacts by name/email",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "ess_recent",
            "description": "List most recent emails",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "scope": {"type": "string"},
                    "account": {"type": "string"},
                    "folder": {"type": "string"},
                    "unread_only": {"type": "boolean"},
                    "limit": {"type": "integer", "minimum": 1}
                }
            }
        }),
        json!({
            "name": "ess_stats",
            "description": "Return ESS database and index stats",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }),
    ]
}

pub fn call_tool(name: &str, arguments: Value) -> Result<Value> {
    match name {
        "ess_search" => ess_search(&arguments),
        "ess_thread" => ess_thread(&arguments),
        "ess_contacts" => ess_contacts(&arguments),
        "ess_recent" => ess_recent(&arguments),
        "ess_stats" => ess_stats(),
        other => Err(anyhow!("unknown tool: {other}")),
    }
}

fn ess_search(arguments: &Value) -> Result<Value> {
    let query = required_string(arguments, "query")?;
    let from = optional_string(arguments, "from");
    let to = optional_string(arguments, "to");
    let since = optional_date(arguments, "since")?;
    let until = optional_date(arguments, "until")?;
    let scope = optional_scope(arguments, "scope")?;
    let account = optional_string(arguments, "account");
    let folder = optional_string(arguments, "folder");
    let limit = optional_usize(arguments, "limit")?.unwrap_or(20);

    let db = open_db()?;
    let index = open_index_with_recovery(&db)?;
    let filters = EmailFilters {
        scope,
        from,
        to,
        since,
        until,
        account,
        folder,
        limit,
        ..EmailFilters::default()
    };

    let results = search::search_emails(&index, &db, &query, &filters)?;
    Ok(json!(results
        .into_iter()
        .map(|result| json!({
            "email": result.email,
            "score": result.score,
            "snippet": result.snippet,
        }))
        .collect::<Vec<_>>()))
}

fn ess_thread(arguments: &Value) -> Result<Value> {
    let conversation_id = required_string(arguments, "conversation_id")?;
    let db = open_db()?;
    let emails = db.get_emails_by_conversation(&conversation_id)?;
    Ok(serde_json::to_value(emails)?)
}

fn ess_contacts(arguments: &Value) -> Result<Value> {
    let query = required_string(arguments, "query")?;
    let db = open_db()?;
    let contacts = db.get_contacts(Some(query.as_str()))?;
    Ok(serde_json::to_value(contacts)?)
}

fn ess_recent(arguments: &Value) -> Result<Value> {
    let scope = optional_scope(arguments, "scope")?;
    let account = optional_string(arguments, "account");
    let folder = optional_string(arguments, "folder");
    let unread_only = optional_bool(arguments, "unread_only").unwrap_or(false);
    let limit = optional_usize(arguments, "limit")?.unwrap_or(20);

    let db = open_db()?;
    let mut emails = db.search_emails(EmailSearchFilters {
        query: None,
        account_id: account,
        account_type: scope_to_account_type(scope),
        folder,
        from_address: None,
        limit,
        offset: 0,
    })?;

    if unread_only {
        emails.retain(|email| !email.is_read.unwrap_or(false));
    }

    Ok(serde_json::to_value(emails)?)
}

fn ess_stats() -> Result<Value> {
    let db = open_db()?;
    let index = open_index_with_recovery(&db)?;

    let db_stats = db.get_stats()?;
    let accounts = db.list_accounts()?;
    let index_stats = index.get_stats()?;

    let account_entries = accounts
        .into_iter()
        .map(|account| {
            let count = db_stats
                .emails_by_account
                .iter()
                .find(|row| row.account_id == account.account_id)
                .map(|row| row.count)
                .unwrap_or(0);
            json!({
                "account_id": account.account_id,
                "email": account.email_address,
                "type": account.account_type.to_string(),
                "count": count,
                "last_sync": account.last_sync
            })
        })
        .collect::<Vec<_>>();

    Ok(json!({
        "total_emails": db_stats.total_emails,
        "accounts": account_entries,
        "index_size": index_stats.index_size_bytes,
        "contact_count": db_stats.total_contacts
    }))
}

fn open_db() -> Result<Database> {
    let db_path = Database::default_db_path().context("resolve ESS database path")?;
    Database::open(&db_path).with_context(|| format!("open ESS database at {}", db_path.display()))
}

fn open_index_with_recovery(db: &Database) -> Result<EmailIndex> {
    let index_path = EmailIndex::default_index_path().context("resolve ESS index path")?;
    match EmailIndex::open(&index_path) {
        Ok(index) => Ok(index),
        Err(open_error) => {
            tracing::warn!(
                "failed to open ESS index at {}: {open_error}; attempting rebuild from SQLite",
                index_path.display()
            );
            rebuild_index_from_db(db, &index_path).with_context(|| {
                format!(
                    "rebuild ESS index at {} after open failure",
                    index_path.display()
                )
            })?;
            EmailIndex::open(&index_path)
                .with_context(|| format!("re-open rebuilt ESS index at {}", index_path.display()))
        }
    }
}

fn rebuild_index_from_db(db: &Database, index_path: &Path) -> Result<usize> {
    if index_path.exists() {
        std::fs::remove_dir_all(index_path).with_context(|| {
            format!(
                "remove corrupted ESS index directory {}",
                index_path.display()
            )
        })?;
    }
    std::fs::create_dir_all(index_path)
        .with_context(|| format!("create ESS index directory {}", index_path.display()))?;
    let mut index = EmailIndex::open(index_path)
        .with_context(|| format!("initialize ESS index at {}", index_path.display()))?;
    let indexed = index
        .reindex(db)
        .context("reindex ESS index from SQLite source-of-truth")?;
    tracing::warn!(
        "rebuilt ESS index at {} with {indexed} indexed emails",
        index_path.display()
    );
    Ok(indexed)
}

fn required_string(arguments: &Value, key: &str) -> Result<String> {
    optional_string(arguments, key).ok_or_else(|| anyhow!("missing required param '{key}'"))
}

fn optional_string(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn optional_bool(arguments: &Value, key: &str) -> Option<bool> {
    arguments.get(key).and_then(Value::as_bool)
}

fn optional_usize(arguments: &Value, key: &str) -> Result<Option<usize>> {
    let Some(raw) = arguments.get(key) else {
        return Ok(None);
    };

    let value = raw
        .as_u64()
        .ok_or_else(|| anyhow!("param '{key}' must be a positive integer"))?;
    if value == 0 {
        return Err(anyhow!("param '{key}' must be greater than zero"));
    }
    Ok(Some(value as usize))
}

fn optional_date(arguments: &Value, key: &str) -> Result<Option<NaiveDate>> {
    optional_string(arguments, key)
        .map(|value| {
            NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
                .with_context(|| format!("param '{key}' must be YYYY-MM-DD"))
        })
        .transpose()
}

fn optional_scope(arguments: &Value, key: &str) -> Result<Scope> {
    let scope = optional_string(arguments, key)
        .map(|value| Scope::from_str(&value).map_err(anyhow::Error::msg))
        .transpose()?
        .unwrap_or(Scope::All);
    Ok(scope)
}

fn scope_to_account_type(scope: Scope) -> Option<String> {
    match scope {
        Scope::Professional => Some("professional".to_string()),
        Scope::Personal => Some("personal".to_string()),
        Scope::All => None,
    }
}
