use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, ToSql};
use serde::Serialize;
use thiserror::Error;

use self::models::{Account, Contact, Email, SyncState};

#[derive(Debug, Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error("json serialization: {0}")]
    Json(#[from] serde_json::Error),

    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Config(String),
}

pub mod migrations;
pub mod models;
pub mod schema;

#[derive(Debug, Clone, Default)]
pub struct EmailSearchFilters {
    pub query: Option<String>,
    pub account_id: Option<String>,
    pub account_type: Option<String>,
    pub folder: Option<String>,
    pub from_address: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountEmailCount {
    pub account_id: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DatabaseStats {
    pub total_accounts: i64,
    pub total_emails: i64,
    pub total_contacts: i64,
    pub emails_by_account: Vec<AccountEmailCount>,
}

pub struct Database {
    conn: Connection,
    path: PathBuf,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        conn.execute("PRAGMA foreign_keys = ON", [])?;

        let mut db = Self {
            conn,
            path: path.to_path_buf(),
        };
        db.initialize()?;
        Ok(db)
    }

    pub fn initialize(&mut self) -> Result<(), DbError> {
        self.run_migrations()
    }

    fn run_migrations(&mut self) -> Result<(), DbError> {
        migrations::migrate(&mut self.conn)
            .map_err(|e| DbError::Config(format!("migration failed: {e}")))
    }

    pub fn default_db_path() -> Result<PathBuf, DbError> {
        let home = dirs::home_dir()
            .ok_or_else(|| DbError::Config("failed to determine home directory".to_string()))?;
        Ok(home.join(".ess").join("ess.db"))
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn insert_account(&self, account: &Account) -> Result<(), DbError> {
        let config_json = account
            .config
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.conn.execute(
            r#"
            INSERT OR REPLACE INTO accounts (
                account_id, email_address, display_name, tenant_id, account_type, enabled, last_sync, config
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                account.account_id,
                account.email_address,
                account.display_name,
                account.tenant_id,
                account.account_type.to_string(),
                account.enabled,
                account.last_sync,
                config_json,
            ],
        )?;

        Ok(())
    }

    pub fn get_account(&self, account_id: &str) -> Result<Option<Account>, DbError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT account_id, email_address, display_name, tenant_id, account_type, enabled, last_sync, config
            FROM accounts
            WHERE account_id = ?
            LIMIT 1
            "#,
        )?;

        let mut rows = stmt.query([account_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Account::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>, DbError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT account_id, email_address, display_name, tenant_id, account_type, enabled, last_sync, config
            FROM accounts
            ORDER BY email_address ASC
            "#,
        )?;

        let accounts = stmt
            .query_map([], Account::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(accounts)
    }

    pub fn remove_account(&self, account_id: &str) -> Result<usize, DbError> {
        let deleted = self
            .conn
            .execute("DELETE FROM accounts WHERE account_id = ?", [account_id])?;
        Ok(deleted)
    }

    pub fn insert_email(&self, email: &Email) -> Result<(), DbError> {
        let to_addresses = serde_json::to_string(&email.to_addresses)?;
        let cc_addresses = serde_json::to_string(&email.cc_addresses)?;
        let bcc_addresses = serde_json::to_string(&email.bcc_addresses)?;
        let categories = serde_json::to_string(&email.categories)?;
        let metadata = email
            .metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;

        self.conn.execute(
            r#"
            INSERT OR REPLACE INTO emails (
                id, internet_message_id, conversation_id, account_id, subject, from_address, from_name,
                to_addresses, cc_addresses, bcc_addresses, body_text, body_html, body_preview,
                received_at, sent_at, importance, is_read, has_attachments, folder, categories,
                flag_status, web_link, metadata
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
            params![
                email.id,
                email.internet_message_id,
                email.conversation_id,
                email.account_id,
                email.subject,
                email.from_address,
                email.from_name,
                to_addresses,
                cc_addresses,
                bcc_addresses,
                email.body_text,
                email.body_html,
                email.body_preview,
                email.received_at,
                email.sent_at,
                email.importance,
                email.is_read,
                email.has_attachments,
                email.folder,
                categories,
                email.flag_status,
                email.web_link,
                metadata,
            ],
        )?;

        Ok(())
    }

    pub fn get_email(&self, id: &str) -> Result<Option<Email>, DbError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, internet_message_id, conversation_id, account_id, subject, from_address, from_name,
                   to_addresses, cc_addresses, bcc_addresses, body_text, body_html, body_preview,
                   received_at, sent_at, importance, is_read, has_attachments, folder, categories,
                   flag_status, web_link, metadata
            FROM emails
            WHERE id = ?
            "#,
        )?;

        let mut rows = stmt.query([id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Email::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn get_emails_by_conversation(&self, conversation_id: &str) -> Result<Vec<Email>, DbError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, internet_message_id, conversation_id, account_id, subject, from_address, from_name,
                   to_addresses, cc_addresses, bcc_addresses, body_text, body_html, body_preview,
                   received_at, sent_at, importance, is_read, has_attachments, folder, categories,
                   flag_status, web_link, metadata
            FROM emails
            WHERE conversation_id = ?
            ORDER BY received_at ASC
            "#,
        )?;

        let emails = stmt
            .query_map([conversation_id], Email::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(emails)
    }

    pub fn search_emails(&self, mut filters: EmailSearchFilters) -> Result<Vec<Email>, DbError> {
        if filters.limit == 0 {
            filters.limit = 50;
        }

        let mut sql = String::from(
            r#"
            SELECT id, internet_message_id, conversation_id, account_id, subject, from_address, from_name,
                   to_addresses, cc_addresses, bcc_addresses, body_text, body_html, body_preview,
                   received_at, sent_at, importance, is_read, has_attachments, folder, categories,
                   flag_status, web_link, metadata
            FROM emails
            WHERE 1 = 1
            "#,
        );
        let mut params_vec: Vec<Box<dyn ToSql>> = Vec::new();

        if let Some(query) = filters.query.filter(|s| !s.trim().is_empty()) {
            sql.push_str(" AND (subject LIKE ? OR body_text LIKE ? OR from_name LIKE ? OR from_address LIKE ?)");
            let pattern = format!("%{query}%");
            params_vec.push(Box::new(pattern.clone()));
            params_vec.push(Box::new(pattern.clone()));
            params_vec.push(Box::new(pattern.clone()));
            params_vec.push(Box::new(pattern));
        }

        if let Some(account_id) = filters.account_id {
            sql.push_str(" AND account_id = ?");
            params_vec.push(Box::new(account_id));
        }

        if let Some(account_type) = filters.account_type {
            sql.push_str(
                " AND account_id IN (SELECT account_id FROM accounts WHERE account_type = ?)",
            );
            params_vec.push(Box::new(account_type));
        }

        if let Some(folder) = filters.folder {
            sql.push_str(" AND folder = ?");
            params_vec.push(Box::new(folder));
        }

        if let Some(from_address) = filters.from_address {
            sql.push_str(" AND from_address = ?");
            params_vec.push(Box::new(from_address));
        }

        sql.push_str(" ORDER BY received_at DESC LIMIT ? OFFSET ?");
        params_vec.push(Box::new(filters.limit as i64));
        params_vec.push(Box::new(filters.offset as i64));

        let params_refs: Vec<&dyn ToSql> = params_vec.iter().map(|v| v.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let results = stmt
            .query_map(params_refs.as_slice(), Email::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(results)
    }

    pub fn get_contacts(&self, query: Option<&str>) -> Result<Vec<Contact>, DbError> {
        let mut sql = String::from(
            r#"
            SELECT email_address, display_name, company, attio_person_id, attio_company_id,
                   message_count, first_seen, last_seen
            FROM contacts
            "#,
        );

        let mut params_vec: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(q) = query.filter(|s| !s.trim().is_empty()) {
            sql.push_str(" WHERE email_address LIKE ? OR display_name LIKE ?");
            let pattern = format!("%{q}%");
            params_vec.push(Box::new(pattern.clone()));
            params_vec.push(Box::new(pattern));
        }
        sql.push_str(" ORDER BY message_count DESC, email_address ASC");

        let params_refs: Vec<&dyn ToSql> = params_vec.iter().map(|v| v.as_ref()).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        let contacts = stmt
            .query_map(params_refs.as_slice(), Contact::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(contacts)
    }

    pub fn update_contact_stats(&self, email_address: &str) -> Result<(), DbError> {
        self.conn.execute(
            r#"
            INSERT INTO contacts (email_address, message_count, first_seen, last_seen)
            VALUES (?, 1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            ON CONFLICT(email_address) DO UPDATE SET
                message_count = contacts.message_count + 1,
                last_seen = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
            "#,
            [email_address],
        )?;
        Ok(())
    }

    pub fn get_sync_state(&self, key: &str) -> Result<Option<SyncState>, DbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT key, value, updated_at FROM sync_state WHERE key = ? LIMIT 1")?;
        let mut rows = stmt.query([key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(SyncState::from_row(row)?))
        } else {
            Ok(None)
        }
    }

    pub fn set_sync_state(&self, key: &str, value: &str) -> Result<(), DbError> {
        self.conn.execute(
            r#"
            INSERT INTO sync_state (key, value, updated_at)
            VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at
            "#,
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_stats(&self) -> Result<DatabaseStats, DbError> {
        let total_accounts: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))?;
        let total_emails: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM emails", [], |row| row.get(0))?;
        let total_contacts: i64 =
            self.conn
                .query_row("SELECT COUNT(*) FROM contacts", [], |row| row.get(0))?;

        let mut stmt = self.conn.prepare(
            "SELECT account_id, COUNT(*) AS count FROM emails GROUP BY account_id ORDER BY count DESC",
        )?;
        let emails_by_account = stmt
            .query_map([], |row| {
                Ok(AccountEmailCount {
                    account_id: row.get(0)?,
                    count: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(DatabaseStats {
            total_accounts,
            total_emails,
            total_contacts,
            emails_by_account,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Database, EmailSearchFilters};
    use crate::db::models::{Account, AccountType, Email};
    use uuid::Uuid;

    fn temp_db_path() -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("ess-test-{}.db", Uuid::new_v4()));
        path
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

    fn sample_email() -> Email {
        Email {
            id: "msg-1".to_string(),
            internet_message_id: Some("<msg-1@example.com>".to_string()),
            conversation_id: Some("thread-1".to_string()),
            account_id: Some("acc-1".to_string()),
            subject: Some("Project kickoff".to_string()),
            from_address: Some("sender@example.com".to_string()),
            from_name: Some("Sender".to_string()),
            to_addresses: vec!["owner@example.com".to_string()],
            cc_addresses: vec![],
            bcc_addresses: vec![],
            body_text: Some("Let us meet tomorrow".to_string()),
            body_html: None,
            body_preview: Some("Let us meet tomorrow".to_string()),
            received_at: "2026-02-01T12:00:00Z".to_string(),
            sent_at: Some("2026-02-01T11:59:00Z".to_string()),
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
    fn database_insert_and_get_email_roundtrip() {
        let path = temp_db_path();
        let db = Database::open(&path).expect("open db");

        db.insert_account(&sample_account())
            .expect("insert account");
        db.insert_email(&sample_email()).expect("insert email");

        let loaded = db
            .get_email("msg-1")
            .expect("get email")
            .expect("email exists");
        assert_eq!(loaded.id, "msg-1");
        assert_eq!(loaded.subject.as_deref(), Some("Project kickoff"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn database_search_and_stats() {
        let path = temp_db_path();
        let db = Database::open(&path).expect("open db");

        db.insert_account(&sample_account())
            .expect("insert account");
        db.insert_email(&sample_email()).expect("insert email");
        db.update_contact_stats("sender@example.com")
            .expect("update contact stats");
        db.set_sync_state("cursor", "abc123")
            .expect("set sync state");

        let results = db
            .search_emails(EmailSearchFilters {
                query: Some("kickoff".to_string()),
                limit: 10,
                ..EmailSearchFilters::default()
            })
            .expect("search emails");
        assert_eq!(results.len(), 1);

        let stats = db.get_stats().expect("db stats");
        assert_eq!(stats.total_accounts, 1);
        assert_eq!(stats.total_emails, 1);
        assert_eq!(stats.total_contacts, 1);

        let state = db.get_sync_state("cursor").expect("get sync state");
        assert_eq!(state.expect("state").value.as_deref(), Some("abc123"));
        let _ = std::fs::remove_file(path);
    }
}
