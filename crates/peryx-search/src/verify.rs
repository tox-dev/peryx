//! Exact verification for matches the n-gram prefilter can only approximate.
//!
//! A substring or a pattern has no literal prefix, so an automaton over an indexed field has to
//! stream the whole term dictionary — a cost that grows with the total indexed text. Verification
//! therefore reads the columnar copy of the indexed window per candidate document instead, which
//! ties the cost to the prefilter's size.

use std::sync::Arc;

use regex::{Regex, RegexBuilder};
use tantivy::columnar::{BytesColumn, StrColumn};
use tantivy::query::{ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, DocSet, Score, SegmentReader, TERMINATED, TantivyError};

#[derive(Debug)]
pub enum Verifier {
    Substring(String),
    Pattern(Regex),
}

impl Verifier {
    /// # Errors
    /// Returns the compilation error for a pattern the regular-expression dialect rejects.
    pub fn pattern(pattern: &str) -> Result<Self, regex::Error> {
        RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map(Self::Pattern)
    }

    fn matches(&self, text: &str) -> bool {
        match self {
            Self::Substring(needle) => text.contains(needle.as_str()),
            Self::Pattern(pattern) => pattern.is_match(text),
        }
    }
}

/// Verifies the documents `candidates` proposes, and only those.
#[derive(Debug, Clone)]
pub struct VerifiedQuery {
    candidates: Arc<dyn Query>,
    field: &'static str,
    verifier: Arc<Verifier>,
}

impl VerifiedQuery {
    pub fn new(candidates: Arc<dyn Query>, field: &'static str, verifier: Verifier) -> Self {
        Self {
            candidates,
            field,
            verifier: Arc::new(verifier),
        }
    }
}

impl Query for VerifiedQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(VerifiedWeight {
            candidates: self.candidates.weight(enable_scoring)?,
            field: self.field,
            verifier: Arc::clone(&self.verifier),
        }))
    }
}

struct VerifiedWeight {
    candidates: Box<dyn Weight>,
    field: &'static str,
    verifier: Arc<Verifier>,
}

impl Weight for VerifiedWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let column = reader.fast_fields().str(self.field)?;
        let column = column.unwrap_or_else(|| StrColumn::wrap(BytesColumn::empty(reader.max_doc())));
        let candidates = self.candidates.scorer(reader, 1.0)?;
        let verified = VerifiedDocSet::new(candidates, column, Arc::clone(&self.verifier));
        Ok(Box::new(ConstScorer::new(verified, boost)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        // Construction already advanced past every unverified document, so the scorer can sit beyond
        // the asked-for one, which `seek` forbids as a target.
        if scorer.doc() > doc || scorer.seek(doc) != doc {
            return Err(TantivyError::InvalidArgument(format!("document {doc} does not match")));
        }
        Ok(Explanation::new("Verified", 1.0))
    }
}

struct VerifiedDocSet {
    candidates: Box<dyn Scorer>,
    column: StrColumn,
    verifier: Arc<Verifier>,
    text: String,
}

impl VerifiedDocSet {
    fn new(candidates: Box<dyn Scorer>, column: StrColumn, verifier: Arc<Verifier>) -> Self {
        let mut docset = Self {
            candidates,
            column,
            verifier,
            text: String::new(),
        };
        let doc = docset.candidates.doc();
        docset.skip_unverified(doc);
        docset
    }

    fn skip_unverified(&mut self, mut doc: DocId) -> DocId {
        while doc != TERMINATED && !self.verified(doc) {
            doc = self.candidates.advance();
        }
        doc
    }

    fn verified(&mut self, doc: DocId) -> bool {
        let ordinal = self.column.term_ords(doc).next();
        self.text.clear();
        ordinal.is_some_and(|ordinal| self.column.ord_to_str(ordinal, &mut self.text).unwrap_or(false))
            && self.verifier.matches(&self.text)
    }
}

impl DocSet for VerifiedDocSet {
    fn advance(&mut self) -> DocId {
        let doc = self.candidates.advance();
        self.skip_unverified(doc)
    }

    fn seek(&mut self, target: DocId) -> DocId {
        let doc = self.candidates.seek(target);
        self.skip_unverified(doc)
    }

    fn doc(&self) -> DocId {
        self.candidates.doc()
    }

    fn size_hint(&self) -> u32 {
        self.candidates.size_hint()
    }
}
