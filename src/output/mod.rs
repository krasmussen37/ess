pub mod json;
pub mod table;

use anyhow::Result;
use serde::Serialize;

use crate::db::models::{Contact, Email};
use crate::db::DatabaseStats;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
}

impl OutputFormat {
    pub fn from_json_flag(json: bool) -> Self {
        if json {
            Self::Json
        } else {
            Self::Table
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResultItem {
    pub email: Email,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

pub fn format_search_results(format: OutputFormat, results: &[SearchResultItem]) -> Result<String> {
    match format {
        OutputFormat::Table => Ok(table::format_search_results(results)),
        OutputFormat::Json => json::format_search_results(results),
    }
}

pub fn format_email(format: OutputFormat, email: &Email) -> Result<String> {
    match format {
        OutputFormat::Table => Ok(table::format_email(email)),
        OutputFormat::Json => json::format_email(email),
    }
}

pub fn format_thread(format: OutputFormat, emails: &[Email]) -> Result<String> {
    match format {
        OutputFormat::Table => Ok(table::format_thread(emails)),
        OutputFormat::Json => json::format_thread(emails),
    }
}

pub fn format_contacts(format: OutputFormat, contacts: &[Contact]) -> Result<String> {
    match format {
        OutputFormat::Table => Ok(table::format_contacts(contacts)),
        OutputFormat::Json => json::format_contacts(contacts),
    }
}

pub fn format_stats(format: OutputFormat, stats: &DatabaseStats) -> Result<String> {
    match format {
        OutputFormat::Table => Ok(table::format_stats(stats)),
        OutputFormat::Json => json::format_stats(stats),
    }
}
