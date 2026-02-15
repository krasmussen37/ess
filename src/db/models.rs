use std::fmt::{Display, Formatter};
use std::str::FromStr;

use rusqlite::{Result as SqlResult, Row};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AccountType {
    Professional,
    Personal,
}

impl Display for AccountType {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Professional => write!(f, "professional"),
            Self::Personal => write!(f, "personal"),
        }
    }
}

impl FromStr for AccountType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "professional" | "pro" => Ok(Self::Professional),
            "personal" => Ok(Self::Personal),
            other => Err(format!("invalid account type: {other}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Email {
    pub id: String,
    pub internet_message_id: Option<String>,
    pub conversation_id: Option<String>,
    pub account_id: Option<String>,
    pub subject: Option<String>,
    pub from_address: Option<String>,
    pub from_name: Option<String>,
    pub to_addresses: Vec<String>,
    pub cc_addresses: Vec<String>,
    pub bcc_addresses: Vec<String>,
    pub body_text: Option<String>,
    pub body_html: Option<String>,
    pub body_preview: Option<String>,
    pub received_at: String,
    pub sent_at: Option<String>,
    pub importance: Option<String>,
    pub is_read: Option<bool>,
    pub has_attachments: Option<bool>,
    pub folder: Option<String>,
    pub categories: Vec<String>,
    pub flag_status: Option<String>,
    pub web_link: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub account_id: String,
    pub email_address: String,
    pub display_name: Option<String>,
    pub tenant_id: Option<String>,
    pub account_type: AccountType,
    pub enabled: bool,
    pub last_sync: Option<String>,
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Contact {
    pub email_address: String,
    pub display_name: Option<String>,
    pub company: Option<String>,
    pub attio_person_id: Option<String>,
    pub attio_company_id: Option<String>,
    pub message_count: i64,
    pub first_seen: Option<String>,
    pub last_seen: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub id: String,
    pub email_id: String,
    pub name: Option<String>,
    pub content_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub is_inline: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SyncState {
    pub key: String,
    pub value: Option<String>,
    pub updated_at: Option<String>,
}

fn parse_json_array(raw: Option<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

fn parse_json_value(raw: Option<String>) -> Option<serde_json::Value> {
    raw.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
}

impl Email {
    pub fn from_row(row: &Row<'_>) -> SqlResult<Self> {
        Ok(Self {
            id: row.get("id")?,
            internet_message_id: row.get("internet_message_id")?,
            conversation_id: row.get("conversation_id")?,
            account_id: row.get("account_id")?,
            subject: row.get("subject")?,
            from_address: row.get("from_address")?,
            from_name: row.get("from_name")?,
            to_addresses: parse_json_array(row.get("to_addresses")?),
            cc_addresses: parse_json_array(row.get("cc_addresses")?),
            bcc_addresses: parse_json_array(row.get("bcc_addresses")?),
            body_text: row.get("body_text")?,
            body_html: row.get("body_html")?,
            body_preview: row.get("body_preview")?,
            received_at: row.get("received_at")?,
            sent_at: row.get("sent_at")?,
            importance: row.get("importance")?,
            is_read: row.get("is_read")?,
            has_attachments: row.get("has_attachments")?,
            folder: row.get("folder")?,
            categories: parse_json_array(row.get("categories")?),
            flag_status: row.get("flag_status")?,
            web_link: row.get("web_link")?,
            metadata: parse_json_value(row.get("metadata")?),
        })
    }
}

impl Account {
    pub fn from_row(row: &Row<'_>) -> SqlResult<Self> {
        let account_type_raw: String = row.get("account_type")?;
        let account_type = AccountType::from_str(&account_type_raw).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                account_type_raw.len(),
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            )
        })?;

        Ok(Self {
            account_id: row.get("account_id")?,
            email_address: row.get("email_address")?,
            display_name: row.get("display_name")?,
            tenant_id: row.get("tenant_id")?,
            account_type,
            enabled: row.get("enabled")?,
            last_sync: row.get("last_sync")?,
            config: parse_json_value(row.get("config")?),
        })
    }
}

impl Contact {
    pub fn from_row(row: &Row<'_>) -> SqlResult<Self> {
        Ok(Self {
            email_address: row.get("email_address")?,
            display_name: row.get("display_name")?,
            company: row.get("company")?,
            attio_person_id: row.get("attio_person_id")?,
            attio_company_id: row.get("attio_company_id")?,
            message_count: row.get("message_count")?,
            first_seen: row.get("first_seen")?,
            last_seen: row.get("last_seen")?,
        })
    }
}

impl Attachment {
    pub fn from_row(row: &Row<'_>) -> SqlResult<Self> {
        Ok(Self {
            id: row.get("id")?,
            email_id: row.get("email_id")?,
            name: row.get("name")?,
            content_type: row.get("content_type")?,
            size_bytes: row.get("size_bytes")?,
            is_inline: row.get("is_inline")?,
        })
    }
}

impl SyncState {
    pub fn from_row(row: &Row<'_>) -> SqlResult<Self> {
        Ok(Self {
            key: row.get("key")?,
            value: row.get("value")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Account, AccountType, Email};

    #[test]
    fn account_type_display_and_parse() {
        assert_eq!(AccountType::Professional.to_string(), "professional");
        assert_eq!(
            "personal"
                .parse::<AccountType>()
                .expect("parse account type"),
            AccountType::Personal
        );
    }

    #[test]
    fn serde_round_trip_models() {
        let account = Account {
            account_id: "acc-1".to_string(),
            email_address: "person@example.com".to_string(),
            display_name: Some("Person".to_string()),
            tenant_id: None,
            account_type: AccountType::Professional,
            enabled: true,
            last_sync: None,
            config: Some(serde_json::json!({"sync": true})),
        };

        let email = Email {
            id: "msg-1".to_string(),
            internet_message_id: Some("<m1@example.com>".to_string()),
            conversation_id: Some("c1".to_string()),
            account_id: Some("acc-1".to_string()),
            subject: Some("Subject".to_string()),
            from_address: Some("sender@example.com".to_string()),
            from_name: Some("Sender".to_string()),
            to_addresses: vec!["to@example.com".to_string()],
            cc_addresses: vec![],
            bcc_addresses: vec![],
            body_text: Some("Hello".to_string()),
            body_html: None,
            body_preview: None,
            received_at: "2026-01-01T00:00:00Z".to_string(),
            sent_at: None,
            importance: Some("normal".to_string()),
            is_read: Some(false),
            has_attachments: Some(false),
            folder: Some("inbox".to_string()),
            categories: vec!["test".to_string()],
            flag_status: None,
            web_link: None,
            metadata: Some(serde_json::json!({"source": "test"})),
        };

        let account_json = serde_json::to_string(&account).expect("serialize account");
        let _: Account = serde_json::from_str(&account_json).expect("deserialize account");

        let email_json = serde_json::to_string(&email).expect("serialize email");
        let _: Email = serde_json::from_str(&email_json).expect("deserialize email");
    }
}
