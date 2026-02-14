use anyhow::Result;
use rusqlite::Connection;

pub fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS accounts (
            account_id TEXT PRIMARY KEY,
            email_address TEXT NOT NULL,
            display_name TEXT,
            tenant_id TEXT,
            account_type TEXT NOT NULL CHECK(account_type IN ('professional', 'personal')),
            enabled BOOLEAN NOT NULL DEFAULT true,
            last_sync TEXT,
            config TEXT
        );

        CREATE TABLE IF NOT EXISTS emails (
            id TEXT PRIMARY KEY,
            internet_message_id TEXT,
            conversation_id TEXT,
            account_id TEXT REFERENCES accounts(account_id),
            subject TEXT,
            from_address TEXT,
            from_name TEXT,
            to_addresses TEXT,
            cc_addresses TEXT,
            bcc_addresses TEXT,
            body_text TEXT,
            body_html TEXT,
            body_preview TEXT,
            received_at TEXT NOT NULL,
            sent_at TEXT,
            importance TEXT,
            is_read BOOLEAN,
            has_attachments BOOLEAN,
            folder TEXT,
            categories TEXT,
            flag_status TEXT,
            web_link TEXT,
            metadata TEXT
        );

        CREATE TABLE IF NOT EXISTS attachments (
            id TEXT PRIMARY KEY,
            email_id TEXT NOT NULL REFERENCES emails(id) ON DELETE CASCADE,
            name TEXT,
            content_type TEXT,
            size_bytes INTEGER,
            is_inline BOOLEAN
        );

        CREATE TABLE IF NOT EXISTS contacts (
            email_address TEXT PRIMARY KEY,
            display_name TEXT,
            company TEXT,
            attio_person_id TEXT,
            attio_company_id TEXT,
            message_count INTEGER NOT NULL DEFAULT 0,
            first_seen TEXT,
            last_seen TEXT
        );

        CREATE TABLE IF NOT EXISTS sync_state (
            key TEXT PRIMARY KEY,
            value TEXT,
            updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );

        CREATE INDEX IF NOT EXISTS idx_emails_account_id ON emails(account_id);
        CREATE INDEX IF NOT EXISTS idx_emails_conversation_id ON emails(conversation_id);
        CREATE INDEX IF NOT EXISTS idx_emails_received_at ON emails(received_at);
        CREATE INDEX IF NOT EXISTS idx_emails_from_address ON emails(from_address);
        CREATE INDEX IF NOT EXISTS idx_contacts_display_name ON contacts(display_name);
        "#,
    )?;

    Ok(())
}
