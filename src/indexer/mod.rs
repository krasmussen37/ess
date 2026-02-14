use std::ops::Bound;
use std::path::{Path, PathBuf};

use chrono::{DateTime as ChronoDateTime, NaiveDate, Utc};
use tantivy::collector::TopDocs;
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, TantivyDocument, Value};
use tantivy::{doc, DateTime as TantivyDateTime, Index, IndexReader, IndexWriter, Term};
use thiserror::Error;

use crate::db::models::Email;
use crate::db::Database;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error(transparent)]
    Tantivy(#[from] tantivy::TantivyError),

    #[error("query parse: {0}")]
    QueryParse(#[from] tantivy::query::QueryParserError),

    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),

    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("invalid timestamp '{value}': expected RFC3339 or YYYY-MM-DD")]
    TimestampParse { value: String },

    #[error("{0}")]
    Config(String),
}

pub mod schema;

#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    pub account_type: Option<String>,
    pub folder: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EmailSearchHit {
    pub email_db_id: String,
    pub score: f32,
    pub subject: Option<String>,
    pub from_name: Option<String>,
    pub from_address: Option<String>,
    pub folder: Option<String>,
    pub account_type: Option<String>,
    pub received_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EmailIndexStats {
    pub doc_count: u64,
    pub index_size_bytes: u64,
}

pub struct EmailIndex {
    index: Index,
    writer: IndexWriter,
    reader: IndexReader,
    fields: schema::EmailSearchFields,
    path: PathBuf,
}

impl EmailIndex {
    pub fn open(path: &Path) -> Result<Self, IndexError> {
        std::fs::create_dir_all(path)?;

        let schema_def = schema::build_schema();
        let mut index = if path.join("meta.json").exists() {
            Index::open_in_dir(path)?
        } else {
            Index::create_in_dir(path, schema_def)?
        };

        schema::ensure_edge_ngram_tokenizer(&mut index)
            .map_err(|e| IndexError::Config(format!("register tokenizer: {e}")))?;
        let fields = schema::fields_from_schema(&index.schema())
            .map_err(|e| IndexError::Config(format!("resolve schema fields: {e}")))?;

        let writer = index.writer(50_000_000)?;
        let reader = index.reader()?;

        Ok(Self {
            index,
            writer,
            reader,
            fields,
            path: path.to_path_buf(),
        })
    }

    pub fn default_index_path() -> Result<PathBuf, IndexError> {
        let home = dirs::home_dir()
            .ok_or_else(|| IndexError::Config("failed to determine home directory".to_string()))?;
        Ok(home.join(".ess").join("index"))
    }

    pub fn add_email(&mut self, email: &Email, account_type: &str) -> Result<(), IndexError> {
        self.index_email_document(email, account_type)?;
        self.commit_and_reload()
    }

    pub fn search(
        &self,
        query: &str,
        filters: &SearchFilters,
        limit: usize,
    ) -> Result<Vec<EmailSearchHit>, IndexError> {
        let requested_limit = limit.max(1);

        let mut parser = QueryParser::for_index(
            &self.index,
            vec![
                self.fields.subject,
                self.fields.from_name,
                self.fields.body_text,
            ],
        );
        parser.set_field_boost(self.fields.subject, schema::SUBJECT_BOOST);
        parser.set_field_boost(self.fields.from_name, schema::FROM_NAME_BOOST);
        parser.set_field_boost(self.fields.body_text, schema::BODY_BOOST);

        let base_query: Box<dyn Query> = if query.trim().is_empty() {
            Box::new(AllQuery)
        } else {
            parser.parse_query(query)?
        };

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, base_query)];

        if let Some(account_type) = filters
            .account_type
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let term =
                Term::from_field_text(self.fields.account_type, &account_type.to_ascii_lowercase());
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        if let Some(folder) = filters
            .folder
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let term = Term::from_field_text(self.fields.folder, folder);
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        let lower_bound = filters
            .since
            .as_deref()
            .map(parse_timestamp)
            .transpose()?
            .map(Bound::Included)
            .unwrap_or(Bound::Unbounded);
        let upper_bound = filters
            .until
            .as_deref()
            .map(parse_timestamp)
            .transpose()?
            .map(Bound::Included)
            .unwrap_or(Bound::Unbounded);

        if !matches!(lower_bound, Bound::Unbounded) || !matches!(upper_bound, Bound::Unbounded) {
            clauses.push((
                Occur::Must,
                Box::new(RangeQuery::new_date_bounds(
                    "received_at".to_string(),
                    lower_bound,
                    upper_bound,
                )),
            ));
        }

        let combined_query: Box<dyn Query> = if clauses.len() == 1 {
            clauses
                .into_iter()
                .next()
                .map(|(_, q)| q)
                .ok_or_else(|| IndexError::Config("missing search clauses".to_string()))?
        } else {
            Box::new(BooleanQuery::new(clauses))
        };

        let searcher = self.reader.searcher();
        let docs = searcher.search(
            combined_query.as_ref(),
            &TopDocs::with_limit(requested_limit),
        )?;

        let mut hits = Vec::with_capacity(docs.len());
        for (score, address) in docs {
            let retrieved_doc: TantivyDocument = searcher.doc(address)?;
            hits.push(EmailSearchHit {
                email_db_id: first_string(&retrieved_doc, self.fields.email_db_id)
                    .unwrap_or_default(),
                score,
                subject: first_string(&retrieved_doc, self.fields.subject),
                from_name: first_string(&retrieved_doc, self.fields.from_name),
                from_address: first_string(&retrieved_doc, self.fields.from_address),
                folder: first_string(&retrieved_doc, self.fields.folder),
                account_type: first_string(&retrieved_doc, self.fields.account_type),
                received_at: retrieved_doc
                    .get_first(self.fields.received_at)
                    .and_then(|value| value.as_datetime())
                    .map(|dt: TantivyDateTime| dt.into_utc().to_string()),
            });
        }

        Ok(hits)
    }

    pub fn reindex(&mut self, db: &Database) -> Result<usize, IndexError> {
        self.writer.delete_all_documents()?;

        let mut stmt = db.conn().prepare(
            r#"
            SELECT
                e.id,
                e.internet_message_id,
                e.conversation_id,
                e.account_id,
                e.subject,
                e.from_address,
                e.from_name,
                e.to_addresses,
                e.cc_addresses,
                e.bcc_addresses,
                e.body_text,
                e.body_html,
                e.body_preview,
                e.received_at,
                e.sent_at,
                e.importance,
                e.is_read,
                e.has_attachments,
                e.folder,
                e.categories,
                e.flag_status,
                e.web_link,
                e.metadata,
                COALESCE(a.account_type, 'personal') AS account_type
            FROM emails e
            LEFT JOIN accounts a ON a.account_id = e.account_id
            ORDER BY e.received_at ASC
            "#,
        )?;

        let mut indexed_count = 0usize;
        let rows = stmt.query_map([], |row| {
            let email = Email::from_row(row)?;
            let account_type: String = row.get("account_type")?;
            Ok((email, account_type))
        })?;

        for row in rows {
            let (email, account_type) = row?;
            self.index_email_document(&email, &account_type)?;
            indexed_count += 1;
        }

        self.commit_and_reload()?;
        Ok(indexed_count)
    }

    pub fn delete_email(&mut self, email_db_id: &str) -> Result<(), IndexError> {
        self.writer
            .delete_term(Term::from_field_text(self.fields.email_db_id, email_db_id));
        self.commit_and_reload()
    }

    pub fn get_stats(&self) -> Result<EmailIndexStats, IndexError> {
        let doc_count = self.reader.searcher().num_docs();
        let index_size_bytes = directory_size(&self.path)?;

        Ok(EmailIndexStats {
            doc_count,
            index_size_bytes,
        })
    }

    fn index_email_document(&mut self, email: &Email, account_type: &str) -> Result<(), IndexError> {
        self.writer
            .delete_term(Term::from_field_text(self.fields.email_db_id, &email.id));

        let mut document = doc!(
            self.fields.email_db_id => email.id.clone(),
            self.fields.account_type => account_type.to_ascii_lowercase(),
        );

        if let Some(subject) = email
            .subject
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            document.add_text(self.fields.subject, subject);
        }
        if let Some(from_name) = email
            .from_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            document.add_text(self.fields.from_name, from_name);
        }
        if let Some(from_address) = email
            .from_address
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            document.add_text(self.fields.from_address, from_address);
        }
        if let Some(body_text) = email
            .body_text
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            document.add_text(self.fields.body_text, body_text);
        }
        if let Some(folder) = email
            .folder
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            document.add_text(self.fields.folder, folder);
        }

        let received_at = parse_timestamp(&email.received_at)?;
        document.add_date(self.fields.received_at, received_at);

        self.writer.add_document(document)?;

        Ok(())
    }

    fn commit_and_reload(&mut self) -> Result<(), IndexError> {
        self.writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }
}

fn parse_timestamp(raw: &str) -> Result<TantivyDateTime, IndexError> {
    if let Ok(parsed) = ChronoDateTime::parse_from_rfc3339(raw) {
        return Ok(TantivyDateTime::from_timestamp_micros(
            parsed.timestamp_micros(),
        ));
    }

    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let midnight = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| IndexError::TimestampParse {
                value: raw.to_string(),
            })?;
        let utc_dt = ChronoDateTime::<Utc>::from_naive_utc_and_offset(midnight, Utc);
        return Ok(TantivyDateTime::from_timestamp_micros(
            utc_dt.timestamp_micros(),
        ));
    }

    Err(IndexError::TimestampParse {
        value: raw.to_string(),
    })
}

fn first_string(document: &TantivyDocument, field: Field) -> Option<String> {
    document
        .get_first(field)
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn directory_size(path: &Path) -> Result<u64, IndexError> {
    let mut total = 0u64;

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;

        if metadata.is_dir() {
            total = total.saturating_add(directory_size(&entry.path())?);
        } else {
            total = total.saturating_add(metadata.len());
        }
    }

    Ok(total)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{EmailIndex, SearchFilters};
    use crate::db::models::{Account, AccountType, Email};
    use crate::db::Database;
    use uuid::Uuid;

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("ess-indexer-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("create temp root");
        root
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
            body_text: Some("Let us meet tomorrow for kickoff".to_string()),
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
    fn indexer_add_search_delete_roundtrip() {
        let root = temp_root();
        let index_path = root.join("index");

        let mut index = EmailIndex::open(&index_path).expect("open index");
        index
            .add_email(&sample_email(), "professional")
            .expect("add email to index");

        let hits = index
            .search("kickoff", &SearchFilters::default(), 10)
            .expect("search indexed email");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].email_db_id, "msg-1");

        index
            .delete_email("msg-1")
            .expect("delete email from index");
        let hits_after_delete = index
            .search("kickoff", &SearchFilters::default(), 10)
            .expect("search after delete");
        assert!(hits_after_delete.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reindex_rebuilds_from_database_source_of_truth() {
        let root = temp_root();
        let db_path = root.join("ess.db");
        let index_path = root.join("index");

        let db = Database::open(&db_path).expect("open db");
        db.insert_account(&sample_account())
            .expect("insert account");
        db.insert_email(&sample_email()).expect("insert email");

        let mut index = EmailIndex::open(&index_path).expect("open index");
        let indexed_count = index.reindex(&db).expect("reindex from db");
        assert_eq!(indexed_count, 1);

        let pro_hits = index
            .search(
                "kickoff",
                &SearchFilters {
                    account_type: Some("professional".to_string()),
                    ..SearchFilters::default()
                },
                10,
            )
            .expect("search with account_type filter");
        assert_eq!(pro_hits.len(), 1);

        let stats = index.get_stats().expect("index stats");
        assert_eq!(stats.doc_count, 1);
        assert!(stats.index_size_bytes > 0);

        let _ = std::fs::remove_dir_all(root);
    }
}
