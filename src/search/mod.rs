pub mod filters;
pub use self::filters::{EmailFilters, Scope, SqlWhereClause};

use anyhow::Result;

use crate::db::models::Email;
use crate::db::Database;
use crate::indexer::{EmailIndex, SearchFilters as IndexSearchFilters};

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub email: Email,
    pub score: f32,
    pub snippet: Option<String>,
}

pub fn search_emails(
    index: &EmailIndex,
    db: &Database,
    query: &str,
    filters: &EmailFilters,
) -> Result<Vec<SearchResult>> {
    let query_text = if query.trim().is_empty() {
        filters.query.as_deref().unwrap_or("")
    } else {
        query
    };

    let requested_limit = filters.limit.saturating_add(filters.offset).max(1);

    let scope = match filters.scope {
        Scope::Professional => Some("professional".to_string()),
        Scope::Personal => Some("personal".to_string()),
        Scope::All => None,
    };

    let index_hits = index.search(
        query_text,
        &IndexSearchFilters {
            account_type: scope,
            folder: filters.folder.clone(),
            since: filters
                .since
                .map(|date| date.format("%Y-%m-%d").to_string()),
            until: filters
                .until
                .map(|date| date.format("%Y-%m-%d").to_string()),
        },
        requested_limit,
    )?;

    let mut results = Vec::with_capacity(index_hits.len());

    for hit in index_hits {
        let Some(email) = db.get_email(&hit.email_db_id)? else {
            continue;
        };

        if let Some(from_address) = filters
            .from
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let from_matches = email
                .from_address
                .as_deref()
                .map(|value| value.eq_ignore_ascii_case(from_address))
                .unwrap_or(false);
            if !from_matches {
                continue;
            }
        }

        if let Some(account_id) = filters
            .account
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let account_matches = email
                .account_id
                .as_deref()
                .map(|value| value == account_id)
                .unwrap_or(false);
            if !account_matches {
                continue;
            }
        }

        if let Some(to_filter) = filters
            .to
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let to_matches = email
                .to_addresses
                .iter()
                .chain(email.cc_addresses.iter())
                .chain(email.bcc_addresses.iter())
                .any(|value| value.eq_ignore_ascii_case(to_filter));
            if !to_matches {
                continue;
            }
        }

        if filters.unread_only && email.is_read.unwrap_or(false) {
            continue;
        }

        results.push(SearchResult {
            snippet: build_snippet(&email, query_text),
            email,
            score: hit.score,
        });
    }

    if filters.offset > 0 {
        return Ok(results.into_iter().skip(filters.offset).collect());
    }

    Ok(results)
}

fn build_snippet(email: &Email, query: &str) -> Option<String> {
    if query.trim().is_empty() {
        return None;
    }

    let body = email
        .body_text
        .as_deref()
        .or(email.body_preview.as_deref())?
        .trim();

    if body.is_empty() {
        return None;
    }

    let query_lower = query.to_ascii_lowercase();
    let body_lower = body.to_ascii_lowercase();

    if let Some(pos) = body_lower.find(&query_lower) {
        let start = floor_char_boundary(body, pos.saturating_sub(50));
        let end = ceil_char_boundary(body, (pos + query_lower.len() + 90).min(body.len()));
        return Some(body[start..end].trim().to_string());
    }

    Some(body.chars().take(140).collect())
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::db::models::{Account, AccountType, Email};
    use crate::db::Database;
    use crate::indexer::EmailIndex;

    use super::filters::{EmailFilters, Scope};
    use super::search_emails;

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("ess-search-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp root");
        root
    }

    fn account(account_id: &str, account_type: AccountType) -> Account {
        Account {
            account_id: account_id.to_string(),
            email_address: format!("{account_id}@example.com"),
            display_name: Some(account_id.to_string()),
            tenant_id: None,
            account_type,
            enabled: true,
            last_sync: None,
            config: None,
        }
    }

    fn email(
        id: &str,
        account_id: &str,
        subject: &str,
        body_text: &str,
        from_name: &str,
        received_at: &str,
    ) -> Email {
        Email {
            id: id.to_string(),
            internet_message_id: Some(format!("<{id}@example.com>")),
            conversation_id: Some("thread-1".to_string()),
            account_id: Some(account_id.to_string()),
            subject: Some(subject.to_string()),
            from_address: Some(format!("{}@example.com", from_name.to_ascii_lowercase())),
            from_name: Some(from_name.to_string()),
            to_addresses: vec!["owner@example.com".to_string()],
            cc_addresses: vec![],
            bcc_addresses: vec![],
            body_text: Some(body_text.to_string()),
            body_html: None,
            body_preview: Some(body_text.chars().take(80).collect()),
            received_at: received_at.to_string(),
            sent_at: Some(received_at.to_string()),
            importance: Some("normal".to_string()),
            is_read: Some(false),
            has_attachments: Some(false),
            folder: Some("inbox".to_string()),
            categories: vec![],
            flag_status: None,
            web_link: None,
            metadata: None,
        }
    }

    #[test]
    fn search_respects_scope_and_bm25_field_boosting() {
        let root = temp_root();
        let db_path = root.join("ess.db");
        let index_path = root.join("index");

        let db = Database::open(&db_path).expect("open db");
        db.insert_account(&account("acc-pro", AccountType::Professional))
            .expect("insert pro account");
        db.insert_account(&account("acc-personal", AccountType::Personal))
            .expect("insert personal account");

        db.insert_email(&email(
            "email-subject",
            "acc-pro",
            "Kickoff notes",
            "Agenda attached",
            "Alice",
            "2026-02-01T10:00:00Z",
        ))
        .expect("insert pro email");

        db.insert_email(&email(
            "email-body",
            "acc-personal",
            "Weekly digest",
            "This body mentions kickoff but not in the subject",
            "Bob",
            "2026-02-01T11:00:00Z",
        ))
        .expect("insert personal email");

        let mut index = EmailIndex::open(&index_path).expect("open index");
        index.reindex(&db).expect("reindex");

        let results = search_emails(
            &index,
            &db,
            "kickoff",
            &EmailFilters {
                limit: 10,
                ..EmailFilters::default()
            },
        )
        .expect("search by kickoff");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].email.id, "email-subject");

        let pro_only = search_emails(
            &index,
            &db,
            "kickoff",
            &EmailFilters {
                scope: Scope::Professional,
                limit: 10,
                ..EmailFilters::default()
            },
        )
        .expect("search with professional scope");

        assert_eq!(pro_only.len(), 1);
        assert_eq!(pro_only[0].email.id, "email-subject");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn snippet_handles_unicode_boundaries() {
        let email = email(
            "unicode-body",
            "acc-pro",
            "Unicode body",
            "Imagine having a thought partner ────────────── where Claude helps.",
            "Alice",
            "2026-02-01T10:00:00Z",
        );

        let snippet = super::build_snippet(&email, "claude");
        assert!(snippet.is_some());
    }
}
