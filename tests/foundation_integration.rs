use std::path::PathBuf;

use ess::db::models::{Account, AccountType, Email};
use ess::db::Database;
use ess::indexer::{EmailIndex, SearchFilters};
use uuid::Uuid;

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("ess-foundation-it-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).expect("create temp test root");
    root
}

fn account(id: &str, email: &str, account_type: AccountType) -> Account {
    Account {
        account_id: id.to_string(),
        email_address: email.to_string(),
        display_name: Some(id.to_string()),
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
    from_name: &str,
    from_address: &str,
    received_at: &str,
) -> Email {
    Email {
        id: id.to_string(),
        internet_message_id: Some(format!("<{id}@example.com>")),
        conversation_id: Some("thread-1".to_string()),
        account_id: Some(account_id.to_string()),
        subject: Some(subject.to_string()),
        from_address: Some(from_address.to_string()),
        from_name: Some(from_name.to_string()),
        to_addresses: vec!["owner@example.com".to_string()],
        cc_addresses: vec![],
        bcc_addresses: vec![],
        body_text: Some("Kickoff agenda and notes".to_string()),
        body_html: None,
        body_preview: Some("Kickoff agenda".to_string()),
        received_at: received_at.to_string(),
        sent_at: Some(received_at.to_string()),
        importance: Some("normal".to_string()),
        is_read: Some(false),
        has_attachments: Some(false),
        folder: Some("inbox".to_string()),
        categories: vec!["work".to_string()],
        flag_status: None,
        web_link: None,
        metadata: None,
    }
}

#[test]
fn foundation_db_and_indexer_integration_smoke_test() {
    let root = temp_root();
    let db_path = root.join("ess.db");
    let index_path = root.join("index");

    let db = Database::open(&db_path).expect("open db");
    db.insert_account(&account(
        "acc-pro",
        "pro@example.com",
        AccountType::Professional,
    ))
    .expect("insert pro account");
    db.insert_account(&account(
        "acc-personal",
        "personal@example.com",
        AccountType::Personal,
    ))
    .expect("insert personal account");

    db.insert_email(&email(
        "email-pro",
        "acc-pro",
        "Project kickoff tomorrow",
        "Alice Manager",
        "alice@example.com",
        "2026-02-01T10:00:00Z",
    ))
    .expect("insert pro email");
    db.insert_email(&email(
        "email-personal",
        "acc-personal",
        "Project kickoff recap",
        "Bob Friend",
        "bob@example.com",
        "2026-02-02T11:00:00Z",
    ))
    .expect("insert personal email");

    let mut index = EmailIndex::open(&index_path).expect("open index");
    let indexed = index.reindex(&db).expect("reindex from db");
    assert_eq!(indexed, 2, "reindex should index all DB emails");

    let subject_hits = index
        .search("kickoff", &SearchFilters::default(), 10)
        .expect("search by subject text");
    assert_eq!(
        subject_hits.len(),
        2,
        "subject search should return both emails"
    );

    let from_name_hits = index
        .search("alice", &SearchFilters::default(), 10)
        .expect("search by from_name");
    assert_eq!(
        from_name_hits.len(),
        1,
        "from_name search should return pro email"
    );
    assert_eq!(from_name_hits[0].email_db_id, "email-pro");

    let scope_hits = index
        .search(
            "kickoff",
            &SearchFilters {
                account_type: Some("professional".to_string()),
                ..SearchFilters::default()
            },
            10,
        )
        .expect("search with scope filter");
    assert_eq!(
        scope_hits.len(),
        1,
        "scope filter should narrow to professional account"
    );
    assert_eq!(scope_hits[0].email_db_id, "email-pro");

    let reindexed = index.reindex(&db).expect("reindex again");
    assert_eq!(
        reindexed, 2,
        "reindex should rebuild deterministically from DB"
    );

    let stats = index.get_stats().expect("get stats");
    assert_eq!(stats.doc_count, 2);

    let _ = std::fs::remove_dir_all(root);
}
