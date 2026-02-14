use anyhow::Result;

use crate::db::models::{Contact, Email};
use crate::db::DatabaseStats;
use crate::output::SearchResultItem;

pub fn format_search_results(results: &[SearchResultItem]) -> Result<String> {
    Ok(serde_json::to_string_pretty(results)?)
}

pub fn format_email(email: &Email) -> Result<String> {
    Ok(serde_json::to_string_pretty(email)?)
}

pub fn format_thread(emails: &[Email]) -> Result<String> {
    Ok(serde_json::to_string_pretty(emails)?)
}

pub fn format_contacts(contacts: &[Contact]) -> Result<String> {
    Ok(serde_json::to_string_pretty(contacts)?)
}

pub fn format_stats(stats: &DatabaseStats) -> Result<String> {
    Ok(serde_json::to_string_pretty(stats)?)
}
