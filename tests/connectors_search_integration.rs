use std::path::PathBuf;

use anyhow::Result;
use ess::connectors::{EmailConnector, JsonArchiveConnector};
use ess::db::models::{Account, AccountType};
use ess::db::Database;
use ess::indexer::EmailIndex;
use ess::search::filters::{EmailFilters, Scope};
use ess::search::search_emails;
use serde_json::json;
use uuid::Uuid;

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("ess-connectors-search-it-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create temp test root");
    root
}

fn account(id: &str, account_type: AccountType) -> Account {
    Account {
        account_id: id.to_string(),
        email_address: format!("{id}@example.com"),
        display_name: Some(id.to_string()),
        tenant_id: None,
        account_type,
        enabled: true,
        last_sync: None,
        config: None,
    }
}

fn write_archive_email(path: &std::path::Path, payload: serde_json::Value) {
    std::fs::write(
        path,
        serde_json::to_string_pretty(&payload).expect("serialize archive payload"),
    )
    .expect("write archive payload");
}

#[tokio::test]
async fn connectors_and_search_end_to_end_validation() -> Result<()> {
    let root = temp_root();
    let db = Database::open(&root.join("ess.db"))?;
    let mut index = EmailIndex::open(&root.join("index"))?;
    let connector = JsonArchiveConnector::new();

    let pro_archive = root.join("pro-archive");
    let personal_archive = root.join("personal-archive");
    std::fs::create_dir_all(&pro_archive)?;
    std::fs::create_dir_all(&personal_archive)?;

    write_archive_email(
        &pro_archive.join("pro-subject.json"),
        json!({
            "id": "pro-subject",
            "subject": "Kickoff schedule",
            "receivedDateTime": "2026-01-10T10:00:00Z",
            "from": { "name": "Alice Manager", "address": "alice@example.com" },
            "toRecipients": [{ "name": "Owner", "address": "owner@example.com" }],
            "importance": "normal",
            "isRead": false,
            "hasAttachments": false,
            "body": { "contentType": "text", "content": "Agenda and action items." },
            "headers": { "Message-ID": "<pro-subject@example.com>", "Thread-Topic": "Kickoff thread" }
        }),
    );
    write_archive_email(
        &pro_archive.join("pro-body.json"),
        json!({
            "id": "pro-body",
            "subject": "Weekly update",
            "receivedDateTime": "2026-01-10T11:00:00Z",
            "from": { "name": "Nora Ops", "address": "nora@example.com" },
            "toRecipients": [{ "name": "Owner", "address": "owner@example.com" }],
            "importance": "normal",
            "isRead": false,
            "hasAttachments": false,
            "body": { "contentType": "text", "content": "This message mentions kickoff in the body only." },
            "headers": { "Message-ID": "<pro-body@example.com>", "Thread-Topic": "Kickoff thread" }
        }),
    );
    write_archive_email(
        &personal_archive.join("personal.json"),
        json!({
            "id": "personal-kickoff",
            "subject": "Kickoff with friends",
            "receivedDateTime": "2026-01-10T12:00:00Z",
            "from": { "name": "Bob Friend", "address": "bob@example.com" },
            "toRecipients": [{ "name": "Owner", "address": "owner@example.com" }],
            "importance": "normal",
            "isRead": false,
            "hasAttachments": false,
            "body": { "contentType": "text", "content": "Personal plans." },
            "headers": { "Message-ID": "<personal@example.com>", "Thread-Topic": "Kickoff social thread" }
        }),
    );

    let pro_account = account("acc-pro", AccountType::Professional);
    let personal_account = account("acc-personal", AccountType::Personal);

    let pro_report = connector
        .import(&db, &mut index, &pro_archive, &pro_account)
        .await?;
    assert_eq!(pro_report.files_processed, 2);
    assert_eq!(pro_report.emails_imported, 2);
    assert!(pro_report.errors.is_empty());

    let personal_report = connector
        .import(&db, &mut index, &personal_archive, &personal_account)
        .await?;
    assert_eq!(personal_report.files_processed, 1);
    assert_eq!(personal_report.emails_imported, 1);
    assert!(personal_report.errors.is_empty());

    assert!(db.get_email("pro-subject")?.is_some());
    assert!(db.get_email("pro-body")?.is_some());
    assert!(db.get_email("personal-kickoff")?.is_some());

    let all_results = search_emails(
        &index,
        &db,
        "kickoff",
        &EmailFilters {
            limit: 10,
            ..EmailFilters::default()
        },
    )?;
    assert_eq!(all_results.len(), 3);
    assert_eq!(
        all_results[0].email.id, "pro-subject",
        "subject match should rank above body-only match"
    );
    assert!(
        all_results[0].score > all_results[1].score,
        "top result should have strongest score"
    );

    let pro_only = search_emails(
        &index,
        &db,
        "kickoff",
        &EmailFilters {
            scope: Scope::Professional,
            limit: 10,
            ..EmailFilters::default()
        },
    )?;
    assert_eq!(pro_only.len(), 2);
    assert!(pro_only
        .iter()
        .all(|result| { result.email.account_id.as_deref() == Some("acc-pro") }));

    let from_filter = search_emails(
        &index,
        &db,
        "kickoff",
        &EmailFilters {
            from: Some("alice@example.com".to_string()),
            limit: 10,
            ..EmailFilters::default()
        },
    )?;
    assert_eq!(from_filter.len(), 1);
    assert_eq!(from_filter[0].email.id, "pro-subject");

    let contacts = db.get_contacts(Some("alice@example.com"))?;
    assert!(!contacts.is_empty(), "sender should be tracked in contacts");

    let _ = std::fs::remove_dir_all(root);
    Ok(())
}
