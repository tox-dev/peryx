use std::sync::Arc;

use tantivy::collector::Count;
use tantivy::query::{AllQuery, EnableScoring, Query as _};
use tantivy::schema::{FAST, Schema, TantivyDocument};
use tantivy::{DocAddress, DocSet as _, Index, IndexWriter, TantivyError};

use crate::verify::{VerifiedQuery, Verifier};

const VERIFY_FIELD: &str = "verify";
const WRITER_MEMORY_BYTES: usize = 15 * 1024 * 1024;

fn indexed(texts: &[&str]) -> Index {
    let mut builder = Schema::builder();
    let verify = builder.add_text_field(VERIFY_FIELD, FAST);
    let index = Index::create_in_ram(builder.build());
    let mut writer: IndexWriter = index
        .writer_with_num_threads(1, WRITER_MEMORY_BYTES)
        .expect("the writer starts");
    for text in texts {
        let mut document = TantivyDocument::new();
        document.add_text(verify, *text);
        writer.add_document(document).expect("the document is written");
    }
    writer.commit().expect("the segment is published");
    index
}

fn substring_query(needle: &str) -> VerifiedQuery {
    VerifiedQuery::new(Arc::new(AllQuery), VERIFY_FIELD, Verifier::Substring(needle.to_owned()))
}

#[test]
fn test_explanation_reports_a_verified_document() {
    let index = indexed(&["alpha", "beta"]);
    let searcher = index.reader().unwrap().searcher();

    let explanation = substring_query("bet")
        .explain(&searcher, DocAddress::new(0, 1))
        .expect("the second document holds the needle");

    assert!(explanation.to_pretty_json().contains("Verified"), "{explanation:?}");
}

#[test]
fn test_explanation_rejects_a_document_verification_excludes() {
    let index = indexed(&["alpha", "beta"]);
    let searcher = index.reader().unwrap().searcher();

    let error = substring_query("bet")
        .explain(&searcher, DocAddress::new(0, 0))
        .expect_err("the first document does not hold the needle");

    assert!(matches!(error, TantivyError::InvalidArgument(_)), "{error}");
}

/// The verified set filters candidates one document at a time, so its size can only shrink from
/// what the candidates report; delegating the hint, rather than fixing it to some other value,
/// keeps that upper bound honest.
#[test]
fn test_verified_doc_set_size_hint_delegates_to_the_candidate_set() {
    let index = indexed(&["alpha", "beta", "gamma"]);
    let searcher = index.reader().unwrap().searcher();
    let query = substring_query("a");
    let weight = query
        .weight(EnableScoring::disabled_from_schema(searcher.schema()))
        .unwrap();
    let segment_reader = searcher.segment_reader(0);
    let candidate_hint = AllQuery
        .weight(EnableScoring::disabled_from_schema(searcher.schema()))
        .unwrap()
        .scorer(segment_reader, 1.0)
        .unwrap()
        .size_hint();

    let scorer = weight.scorer(segment_reader, 1.0).unwrap();

    assert_eq!(scorer.size_hint(), candidate_hint);
}

#[test]
fn test_a_missing_verification_column_verifies_nothing() {
    let index = indexed(&["alpha", "beta"]);
    let searcher = index.reader().unwrap().searcher();
    let query = VerifiedQuery::new(Arc::new(AllQuery), "absent", Verifier::Substring("bet".to_owned()));

    assert_eq!(searcher.search(&query, &Count).unwrap(), 0);
}
