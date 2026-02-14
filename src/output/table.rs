use chrono::{DateTime, Utc};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::db::models::{Contact, Email};
use crate::db::DatabaseStats;
use crate::output::SearchResultItem;

const FROM_WIDTH: usize = 24;
const SUBJECT_WIDTH: usize = 56;
const DATE_WIDTH: usize = 12;
const SCORE_WIDTH: usize = 7;

pub fn format_search_results(results: &[SearchResultItem]) -> String {
    if results.is_empty() {
        return "No emails found.".to_string();
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{:<from$}  {:<subject$}  {:<date$}  {:>score$}\n",
        "From",
        "Subject",
        "Date",
        "Score",
        from = FROM_WIDTH,
        subject = SUBJECT_WIDTH,
        date = DATE_WIDTH,
        score = SCORE_WIDTH
    ));
    out.push_str(&format!(
        "{}  {}  {}  {}\n",
        "-".repeat(FROM_WIDTH),
        "-".repeat(SUBJECT_WIDTH),
        "-".repeat(DATE_WIDTH),
        "-".repeat(SCORE_WIDTH)
    ));

    for item in results {
        let from = truncate_for_width(
            item.email
                .from_name
                .as_deref()
                .or(item.email.from_address.as_deref())
                .unwrap_or("(unknown)"),
            FROM_WIDTH,
        );
        let subject = truncate_for_width(
            item.email.subject.as_deref().unwrap_or("(no subject)"),
            SUBJECT_WIDTH,
        );
        let date = truncate_for_width(&relative_date(&item.email.received_at), DATE_WIDTH);
        let score = item
            .score
            .map(|v| format!("{v:.2}"))
            .unwrap_or_else(|| "-".to_string());

        out.push_str(&format!(
            "{:<from$}  {:<subject$}  {:<date$}  {:>score$}\n",
            from,
            subject,
            date,
            score,
            from = FROM_WIDTH,
            subject = SUBJECT_WIDTH,
            date = DATE_WIDTH,
            score = SCORE_WIDTH
        ));
    }

    out
}

pub fn format_email(email: &Email) -> String {
    let mut out = String::new();
    out.push_str(&format!("ID: {}\n", email.id));
    out.push_str(&format!(
        "Subject: {}\n",
        email.subject.as_deref().unwrap_or("(no subject)")
    ));
    out.push_str(&format!(
        "From: {} <{}>\n",
        email.from_name.as_deref().unwrap_or("(unknown)"),
        email.from_address.as_deref().unwrap_or("(unknown)")
    ));
    if !email.to_addresses.is_empty() {
        out.push_str(&format!("To: {}\n", email.to_addresses.join(", ")));
    }
    if !email.cc_addresses.is_empty() {
        out.push_str(&format!("CC: {}\n", email.cc_addresses.join(", ")));
    }
    out.push_str(&format!(
        "Date: {} ({})\n",
        email.received_at,
        relative_date(&email.received_at)
    ));
    out.push_str(&format!(
        "Importance: {}\n",
        colorize_importance(email.importance.as_deref().unwrap_or("normal"))
    ));
    out.push_str(&format!(
        "Folder: {}\n",
        email.folder.as_deref().unwrap_or("(unknown)")
    ));

    if let Some(conversation_id) = &email.conversation_id {
        out.push_str(&format!("Conversation: {conversation_id}\n"));
    }

    out.push('\n');
    out.push_str("Body\n");
    out.push_str("----\n");

    let body = email
        .body_text
        .as_deref()
        .or(email.body_preview.as_deref())
        .unwrap_or("(empty)");
    out.push_str(body);
    out.push('\n');
    out
}

pub fn format_thread(emails: &[Email]) -> String {
    if emails.is_empty() {
        return "Thread has no messages.".to_string();
    }

    let mut out = String::new();
    for (idx, email) in emails.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
            out.push_str(&"-".repeat(80));
            out.push('\n');
        }
        out.push_str(&format_email(email));
    }
    out
}

pub fn format_contacts(contacts: &[Contact]) -> String {
    if contacts.is_empty() {
        return "No contacts found.".to_string();
    }

    let mut out = String::new();
    out.push_str("Contact                     Messages  Last Seen\n");
    out.push_str("--------------------------  --------  --------------------\n");
    for contact in contacts {
        let label = match &contact.display_name {
            Some(display_name) => format!("{display_name} <{}>", contact.email_address),
            None => contact.email_address.clone(),
        };

        out.push_str(&format!(
            "{:<26}  {:>8}  {}\n",
            truncate_for_width(&label, 26),
            contact.message_count,
            contact.last_seen.as_deref().unwrap_or("-")
        ));
    }

    out
}

pub fn format_stats(stats: &DatabaseStats) -> String {
    let mut out = String::new();
    out.push_str("ESS Stats\n");
    out.push_str("=========\n");
    out.push_str(&format!("Accounts: {}\n", stats.total_accounts));
    out.push_str(&format!("Emails:   {}\n", stats.total_emails));
    out.push_str(&format!("Contacts: {}\n", stats.total_contacts));

    if !stats.emails_by_account.is_empty() {
        out.push('\n');
        out.push_str("Emails by account\n");
        out.push_str("-----------------\n");
        for row in &stats.emails_by_account {
            out.push_str(&format!("{:<24} {:>8}\n", row.account_id, row.count));
        }
    }

    out
}

fn colorize_importance(raw: &str) -> String {
    let normalized = raw.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "high" => format!("\u{1b}[31m{}\u{1b}[0m", raw),
        "low" => format!("\u{1b}[32m{}\u{1b}[0m", raw),
        _ => format!("\u{1b}[33m{}\u{1b}[0m", raw),
    }
}

fn relative_date(input: &str) -> String {
    let parsed = match DateTime::parse_from_rfc3339(input) {
        Ok(value) => value.with_timezone(&Utc),
        Err(_) => return input.to_string(),
    };

    let now = Utc::now();
    let delta = now.signed_duration_since(parsed);
    if delta.num_seconds() < 0 {
        return "in future".to_string();
    }
    if delta.num_minutes() < 1 {
        return "just now".to_string();
    }
    if delta.num_hours() < 1 {
        return format!("{}m ago", delta.num_minutes());
    }
    if delta.num_hours() < 24 {
        return format!("{}h ago", delta.num_hours());
    }
    if delta.num_days() == 1 {
        return "yesterday".to_string();
    }
    if delta.num_days() < 7 {
        return format!("{}d ago", delta.num_days());
    }
    parsed.format("%Y-%m-%d").to_string()
}

fn truncate_for_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_string();
    }

    if max_width <= 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut width = 0usize;
    for c in value.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if width + cw + 1 > max_width {
            break;
        }
        out.push(c);
        width += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use crate::db::models::Email;
    use crate::output::SearchResultItem;

    use super::{format_email, format_search_results};

    fn sample_email() -> Email {
        Email {
            id: "msg-1".to_string(),
            internet_message_id: None,
            conversation_id: Some("thread-1".to_string()),
            account_id: Some("acc-1".to_string()),
            subject: Some("A very long subject line that should be truncated in table output because it exceeds width".to_string()),
            from_address: Some("sender@example.com".to_string()),
            from_name: Some("Sender Name".to_string()),
            to_addresses: vec!["owner@example.com".to_string()],
            cc_addresses: vec![],
            bcc_addresses: vec![],
            body_text: Some("Body".to_string()),
            body_html: None,
            body_preview: Some("Preview".to_string()),
            received_at: (Utc::now() - Duration::hours(2)).to_rfc3339(),
            sent_at: None,
            importance: Some("high".to_string()),
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
    fn table_search_output_has_headers() {
        let rendered = format_search_results(&[SearchResultItem {
            email: sample_email(),
            score: Some(12.34),
        }]);
        assert!(rendered.contains("From"));
        assert!(rendered.contains("Subject"));
        assert!(rendered.contains("Score"));
    }

    #[test]
    fn full_email_output_contains_body() {
        let rendered = format_email(&sample_email());
        assert!(rendered.contains("Body"));
        assert!(rendered.contains("Importance"));
    }
}
