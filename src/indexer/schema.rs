use anyhow::{anyhow, Result};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer};
use tantivy::Index;

pub const SUBJECT_BOOST: f32 = 5.0;
pub const FROM_NAME_BOOST: f32 = 3.0;
pub const BODY_BOOST: f32 = 1.0;

pub const EDGE_NGRAM_TOKENIZER: &str = "edge_ngram";

#[derive(Debug, Clone, Copy)]
pub struct EmailSearchFields {
    pub subject: Field,
    pub from_name: Field,
    pub from_address: Field,
    pub body_text: Field,
    pub received_at: Field,
    pub account_type: Field,
    pub folder: Field,
    pub email_db_id: Field,
}

pub fn build_schema() -> Schema {
    let mut schema = Schema::builder();

    let tokenized_text = TextOptions::default()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(EDGE_NGRAM_TOKENIZER)
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        )
        .set_stored();

    schema.add_text_field("subject", tokenized_text.clone());
    schema.add_text_field("from_name", tokenized_text.clone());
    schema.add_text_field("from_address", tokenized_text.clone());
    schema.add_text_field("body_text", tokenized_text);

    schema.add_date_field("received_at", INDEXED | STORED);
    schema.add_text_field("account_type", STRING | STORED);
    schema.add_text_field("folder", STRING | STORED);
    schema.add_text_field("email_db_id", STRING | STORED);

    schema.build()
}

pub fn fields_from_schema(schema: &Schema) -> Result<EmailSearchFields> {
    let get = |name: &str| -> Result<Field> {
        schema
            .get_field(name)
            .map_err(|_| anyhow!("missing field in Tantivy schema: {name}"))
    };

    Ok(EmailSearchFields {
        subject: get("subject")?,
        from_name: get("from_name")?,
        from_address: get("from_address")?,
        body_text: get("body_text")?,
        received_at: get("received_at")?,
        account_type: get("account_type")?,
        folder: get("folder")?,
        email_db_id: get("email_db_id")?,
    })
}

pub fn ensure_edge_ngram_tokenizer(index: &mut Index) -> Result<()> {
    let edge_ngrams = TextAnalyzer::builder(NgramTokenizer::new(2, 20, false)?)
        .filter(LowerCaser)
        .build();

    index
        .tokenizers()
        .register(EDGE_NGRAM_TOKENIZER, edge_ngrams);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_includes_required_fields() {
        let schema = build_schema();
        let fields = fields_from_schema(&schema).expect("extract fields from schema");
        let _ = fields.subject;
        let _ = fields.from_name;
        let _ = fields.from_address;
        let _ = fields.body_text;
        let _ = fields.received_at;
        let _ = fields.account_type;
        let _ = fields.folder;
        let _ = fields.email_db_id;
    }

    #[test]
    fn boost_constants_match_requirements() {
        assert_eq!(SUBJECT_BOOST, 5.0);
        assert_eq!(FROM_NAME_BOOST, 3.0);
        assert_eq!(BODY_BOOST, 1.0);
    }
}
