use std::ops::Bound;
use std::str::FromStr;

use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, Utc};
use tantivy::query::{AllQuery, BooleanQuery, Occur, Query, QueryParser, RangeQuery, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{DateTime as TantivyDateTime, Index, Term};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scope {
    Professional,
    Personal,
    #[default]
    All,
}

impl Scope {
    fn account_type_filter(self) -> Option<&'static str> {
        match self {
            Self::Professional => Some("professional"),
            Self::Personal => Some("personal"),
            Self::All => None,
        }
    }
}

impl FromStr for Scope {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "professional" | "pro" => Ok(Self::Professional),
            "personal" => Ok(Self::Personal),
            "all" => Ok(Self::All),
            other => Err(format!("invalid scope: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlWhereClause {
    pub clause: String,
    pub params: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailFilters {
    pub query: Option<String>,
    pub scope: Scope,
    pub from: Option<String>,
    pub to: Option<String>,
    pub since: Option<NaiveDate>,
    pub until: Option<NaiveDate>,
    pub account: Option<String>,
    pub folder: Option<String>,
    pub unread_only: bool,
    pub limit: usize,
    pub offset: usize,
}

impl Default for EmailFilters {
    fn default() -> Self {
        Self {
            query: None,
            scope: Scope::All,
            from: None,
            to: None,
            since: None,
            until: None,
            account: None,
            folder: None,
            unread_only: false,
            limit: 20,
            offset: 0,
        }
    }
}

impl EmailFilters {
    pub fn to_tantivy_query(&self, index: &Index) -> Result<BooleanQuery> {
        let schema = index.schema();
        let get_field = |name: &str| schema.get_field(name).ok();

        let subject_field = get_field("subject");
        let from_name_field = get_field("from_name");
        let body_text_field = get_field("body_text");

        let query_fields: Vec<_> = [subject_field, from_name_field, body_text_field]
            .into_iter()
            .flatten()
            .collect();

        let base_query: Box<dyn Query> = if let Some(query) = self
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if query_fields.is_empty() {
                return Err(anyhow!(
                    "cannot build Tantivy query: schema does not contain queryable text fields"
                ));
            }

            let mut parser = QueryParser::for_index(index, query_fields);
            if let Some(field) = subject_field {
                parser.set_field_boost(field, 5.0);
            }
            if let Some(field) = from_name_field {
                parser.set_field_boost(field, 3.0);
            }
            if let Some(field) = body_text_field {
                parser.set_field_boost(field, 1.0);
            }

            parser
                .parse_query(query)
                .map_err(|error| anyhow!("failed to parse query '{query}': {error}"))?
        } else {
            Box::new(AllQuery)
        };

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, base_query)];

        if let (Some(account_type), Some(field)) =
            (self.scope.account_type_filter(), get_field("account_type"))
        {
            let term = Term::from_field_text(field, account_type);
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        if let (Some(folder), Some(field)) = (
            self.folder
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            get_field("folder"),
        ) {
            let term = Term::from_field_text(field, folder);
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        if let (Some(from_address), Some(field)) = (
            self.from
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            get_field("from_address"),
        ) {
            let term = Term::from_field_text(field, &from_address.to_ascii_lowercase());
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        if let (Some(to_address), Some(field)) = (
            self.to
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            get_field("to_addresses"),
        ) {
            let term = Term::from_field_text(field, &to_address.to_ascii_lowercase());
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        if let (Some(account_id), Some(field)) = (
            self.account
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            get_field("account_id"),
        ) {
            let term = Term::from_field_text(field, account_id);
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
            ));
        }

        if let Some(field) = get_field("received_at") {
            let lower_bound = self
                .since
                .map(start_of_day)
                .transpose()?
                .map(Bound::Included)
                .unwrap_or(Bound::Unbounded);
            let upper_bound = self
                .until
                .map(end_of_day)
                .transpose()?
                .map(Bound::Included)
                .unwrap_or(Bound::Unbounded);

            if !matches!(lower_bound, Bound::Unbounded) || !matches!(upper_bound, Bound::Unbounded)
            {
                clauses.push((
                    Occur::Must,
                    Box::new(RangeQuery::new_date_bounds(
                        schema.get_field_name(field).to_string(),
                        lower_bound,
                        upper_bound,
                    )),
                ));
            }
        }

        if self.unread_only {
            if let Some(field) = get_field("is_read") {
                let term = Term::from_field_text(field, "false");
                clauses.push((
                    Occur::Must,
                    Box::new(TermQuery::new(term, IndexRecordOption::Basic)),
                ));
            }
        }

        Ok(BooleanQuery::new(clauses))
    }

    pub fn to_sql_where(&self) -> SqlWhereClause {
        let mut fragments: Vec<String> = Vec::new();
        let mut params: Vec<String> = Vec::new();

        if let Some(query) = self
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            fragments.push(
                "(subject LIKE ? OR body_text LIKE ? OR from_name LIKE ? OR from_address LIKE ?)"
                    .to_string(),
            );
            let pattern = format!("%{query}%");
            params.push(pattern.clone());
            params.push(pattern.clone());
            params.push(pattern.clone());
            params.push(pattern);
        }

        if let Some(account_type) = self.scope.account_type_filter() {
            fragments.push(
                "account_id IN (SELECT account_id FROM accounts WHERE account_type = ?)"
                    .to_string(),
            );
            params.push(account_type.to_string());
        }

        if let Some(from_address) = self
            .from
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            fragments.push("LOWER(from_address) = LOWER(?)".to_string());
            params.push(from_address.to_string());
        }

        if let Some(to_address) = self
            .to
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            fragments.push(
                "(LOWER(to_addresses) LIKE LOWER(?) OR LOWER(cc_addresses) LIKE LOWER(?) OR LOWER(bcc_addresses) LIKE LOWER(?))"
                    .to_string(),
            );
            let pattern = format!("%{to_address}%");
            params.push(pattern.clone());
            params.push(pattern.clone());
            params.push(pattern);
        }

        if let Some(since) = self.since {
            fragments.push("DATE(received_at) >= DATE(?)".to_string());
            params.push(since.to_string());
        }

        if let Some(until) = self.until {
            fragments.push("DATE(received_at) <= DATE(?)".to_string());
            params.push(until.to_string());
        }

        if let Some(account_id) = self
            .account
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            fragments.push("account_id = ?".to_string());
            params.push(account_id.to_string());
        }

        if let Some(folder) = self
            .folder
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            fragments.push("folder = ?".to_string());
            params.push(folder.to_string());
        }

        if self.unread_only {
            fragments.push("COALESCE(is_read, 0) = 0".to_string());
        }

        SqlWhereClause {
            clause: if fragments.is_empty() {
                "1 = 1".to_string()
            } else {
                fragments.join(" AND ")
            },
            params,
        }
    }
}

fn start_of_day(date: NaiveDate) -> Result<TantivyDateTime> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| anyhow!("invalid lower bound date: {date}"))?;
    let dt = DateTime::<Utc>::from_naive_utc_and_offset(midnight, Utc);
    Ok(TantivyDateTime::from_timestamp_micros(
        dt.timestamp_micros(),
    ))
}

fn end_of_day(date: NaiveDate) -> Result<TantivyDateTime> {
    let latest = date
        .and_hms_micro_opt(23, 59, 59, 999_999)
        .ok_or_else(|| anyhow!("invalid upper bound date: {date}"))?;
    let dt = DateTime::<Utc>::from_naive_utc_and_offset(latest, Utc);
    Ok(TantivyDateTime::from_timestamp_micros(
        dt.timestamp_micros(),
    ))
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;
    use tantivy::Index;

    use crate::indexer::schema::{build_schema, ensure_edge_ngram_tokenizer};

    use super::{EmailFilters, Scope};

    #[test]
    fn scope_parses_aliases() {
        assert_eq!(
            "pro".parse::<Scope>().expect("parse pro"),
            Scope::Professional
        );
        assert_eq!(
            "professional".parse::<Scope>().expect("parse professional"),
            Scope::Professional
        );
        assert_eq!(
            "personal".parse::<Scope>().expect("parse personal"),
            Scope::Personal
        );
        assert_eq!("all".parse::<Scope>().expect("parse all"), Scope::All);
        assert!("none".parse::<Scope>().is_err());
    }

    #[test]
    fn default_filters_match_contract() {
        let filters = EmailFilters::default();
        assert_eq!(filters.scope, Scope::All);
        assert_eq!(filters.limit, 20);
        assert_eq!(filters.offset, 0);
        assert!(!filters.unread_only);
    }

    #[test]
    fn sql_where_contains_expected_clauses() {
        let filters = EmailFilters {
            query: Some("kickoff".to_string()),
            scope: Scope::Professional,
            from: Some("alice@example.com".to_string()),
            to: Some("owner@example.com".to_string()),
            since: Some(NaiveDate::from_ymd_opt(2026, 1, 1).expect("valid since")),
            until: Some(NaiveDate::from_ymd_opt(2026, 1, 31).expect("valid until")),
            account: Some("acc-pro".to_string()),
            folder: Some("inbox".to_string()),
            unread_only: true,
            limit: 20,
            offset: 0,
        };

        let where_clause = filters.to_sql_where();
        assert!(where_clause.clause.contains("subject LIKE ?"));
        assert!(where_clause.clause.contains("account_type = ?"));
        assert!(where_clause
            .clause
            .contains("LOWER(from_address) = LOWER(?)"));
        assert!(where_clause.clause.contains("DATE(received_at) >= DATE(?)"));
        assert!(where_clause.clause.contains("account_id = ?"));
        assert!(where_clause.clause.contains("folder = ?"));
        assert!(where_clause.clause.contains("COALESCE(is_read, 0) = 0"));
        assert_eq!(where_clause.params.len(), 13);
    }

    #[test]
    fn tantivy_query_builds_with_filters() {
        let schema = build_schema();
        let mut index = Index::create_in_ram(schema);
        ensure_edge_ngram_tokenizer(&mut index).expect("register edge ngram tokenizer");

        let filters = EmailFilters {
            query: Some("kickoff".to_string()),
            scope: Scope::Professional,
            from: Some("alice@example.com".to_string()),
            since: Some(NaiveDate::from_ymd_opt(2026, 1, 1).expect("valid since")),
            until: Some(NaiveDate::from_ymd_opt(2026, 1, 31).expect("valid until")),
            folder: Some("inbox".to_string()),
            ..EmailFilters::default()
        };

        let query = filters
            .to_tantivy_query(&index)
            .expect("build tantivy query");
        assert!(!query.clauses().is_empty());
    }
}
