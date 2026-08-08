use std::sync::Arc;

use peryx_core::{Ecosystem, LexiconRegistry};

use super::{ALT_WORDS, Stores};
use crate::context::IndexerCtx;
use crate::{
    AvailabilityFilter, PackageDocument, PackageIndexer, PackageSearch, PackageSource, SearchAccess,
    SearchAccessPattern, SearchError, SearchParams, SourceFilter,
};

/// A stand-in ecosystem indexer that yields one document of a given ecosystem regardless of context.
struct OneDoc {
    name: &'static str,
    ecosystem: &'static str,
}

impl PackageIndexer for OneDoc {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok(vec![PackageDocument {
            display_name: self.name.to_owned(),
            normalized_name: self.name.to_owned(),
            route: "root".to_owned(),
            index: "root".to_owned(),
            ecosystem: self.ecosystem.to_owned(),
            source: PackageSource::Cached,
            available_locally: false,
            summary: None,
            text: self.name.to_owned(),
        }])
    }
}

#[test]
fn test_add_indexer_composes_both_ecosystems_with_localized_labels() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let mut lexicons = LexiconRegistry::default();
    lexicons.register(Ecosystem::new("beta"), &ALT_WORDS);
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "pyalpha",
        ecosystem: "alpha",
    }));
    search.add_indexer(Arc::new(OneDoc {
        name: "beta-collection",
        ecosystem: "beta",
    }));

    let all = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: String::new(),
                ..SearchParams::default()
            },
        )
        .unwrap();

    let alpha = all
        .results
        .iter()
        .find(|result| result.display_name == "pyalpha")
        .unwrap();
    let beta = all
        .results
        .iter()
        .find(|result| result.display_name == "beta-collection")
        .unwrap();
    // Each result is labeled in its ecosystem's own word, resolved server-side from the lexicon.
    assert_eq!(alpha.type_label, "package");
    assert_eq!(beta.type_label, "component");
}

#[test]
fn test_search_rebuilds_after_epoch_bump() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let mut lexicons = LexiconRegistry::default();
    lexicons.register(Ecosystem::new("other"), &ALT_WORDS);
    let mut search = PackageSearch::in_memory();
    let params = SearchParams::default();
    let before = search.search(&stores.ctx(&lexicons), params.clone()).unwrap().total;

    search.add_indexer(Arc::new(OneDoc {
        name: "beta-collection",
        ecosystem: "beta",
    }));
    search.bump_epoch();

    assert_eq!(
        (before, search.search(&stores.ctx(&lexicons), params).unwrap().total),
        (0, 1)
    );
}

#[test]
fn test_search_applies_source_and_route_filters() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "demo",
        ecosystem: "alpha",
    }));

    for (source, route, expected) in [
        (SourceFilter::Cached, Some("root"), 1),
        (SourceFilter::Uploaded, Some("root"), 0),
        (SourceFilter::Override, Some("root"), 0),
        (SourceFilter::Cached, Some("other"), 0),
    ] {
        assert_eq!(
            search
                .search(
                    &stores.ctx(&lexicons),
                    SearchParams {
                        route: route.map(str::to_owned),
                        source,
                        ..SearchParams::default()
                    },
                )
                .unwrap()
                .total,
            expected,
            "{source:?} {route:?}"
        );
    }
}

#[test]
fn test_search_handles_empty_and_escaped_regex_queries() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "demo",
        ecosystem: "alpha",
    }));

    for (query, expected) in [("re:", 1), ("+", 0)] {
        assert_eq!(
            search
                .search(
                    &stores.ctx(&lexicons),
                    SearchParams {
                        query: query.to_owned(),
                        ..SearchParams::default()
                    },
                )
                .unwrap()
                .total,
            expected,
            "{query}"
        );
    }
}

#[test]
fn test_search_rejects_invalid_regex_queries() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let search = PackageSearch::in_memory();

    let error = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: "re:[".to_owned(),
                ..SearchParams::default()
            },
        )
        .expect_err("an invalid regular expression should be rejected");

    assert!(error.is_bad_request());
}

#[test]
fn test_search_folds_case_for_non_ascii_text() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "ZÜRICH",
        ecosystem: "alpha",
    }));

    for (case, query) in [
        ("uppercase accented substring term", "ZÜRICH"),
        ("lowercase accented substring term", "zürich"),
        ("accented regex over the raw field", "re:zürich"),
    ] {
        let response = search
            .search(
                &stores.ctx(&lexicons),
                SearchParams {
                    query: query.to_owned(),
                    ..SearchParams::default()
                },
            )
            .unwrap();

        assert_eq!(
            response
                .results
                .iter()
                .map(|result| result.display_name.as_str())
                .collect::<Vec<_>>(),
            ["ZÜRICH"],
            "{case}"
        );
    }
}

/// Two documents whose text shares every 12-gram of a 13-character query: one holds the overlapping
/// grams in separate spans without the whole substring, the other holds the substring intact. The
/// n-gram prefilter accepts both, so the response proves only the true substring survives verification.
struct SubstringDocs;

impl PackageIndexer for SubstringDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok([
            ("separated", "abcdefghijkl xx bcdefghijklm"),
            ("whole", "zzabcdefghijklmzz"),
        ]
        .into_iter()
        .map(|(name, text)| PackageDocument {
            display_name: name.to_owned(),
            normalized_name: name.to_owned(),
            route: "root".to_owned(),
            index: "root".to_owned(),
            ecosystem: "alpha".to_owned(),
            source: PackageSource::Cached,
            available_locally: false,
            summary: None,
            text: text.to_owned(),
        })
        .collect())
    }
}

#[test]
fn test_long_query_verifies_the_full_substring_after_the_ngram_prefilter() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(SubstringDocs));

    let response = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: "abcdefghijklm".to_owned(),
                ..SearchParams::default()
            },
        )
        .unwrap();

    // The separated document satisfies both 12-grams yet lacks the 13-character substring, so only the
    // whole document matches, and the total counts it alone.
    assert_eq!(
        (
            response.total,
            response
                .results
                .iter()
                .map(|result| result.display_name.as_str())
                .collect::<Vec<_>>()
        ),
        (1, vec!["whole"])
    );
}

#[test]
fn test_authorized_search_filters_before_counting_and_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(AccessDocs));

    for (case, patterns, expected) in [
        ("no patterns", vec![], (0, vec![])),
        ("one pattern", vec![("private", "team/*")], (1, vec!["team/app"])),
        (
            "union",
            vec![("private", "team/*"), ("public", "*")],
            (2, vec!["team/app"]),
        ),
    ] {
        let response = search
            .search_authorized(
                &stores.ctx(&lexicons),
                SearchParams {
                    page_size: 1,
                    ..SearchParams::default()
                },
                &SearchAccess::new(
                    patterns
                        .into_iter()
                        .map(|(route, glob)| SearchAccessPattern {
                            route: route.to_owned(),
                            glob: glob.to_owned(),
                        })
                        .collect(),
                ),
            )
            .unwrap();

        assert_eq!(
            (
                response.total,
                response
                    .results
                    .iter()
                    .map(|result| result.normalized_name.as_str())
                    .collect::<Vec<_>>()
            ),
            expected,
            "{case}"
        );
    }
}

#[test]
fn test_from_query_parses_and_validates_the_availability_filter() {
    for (query, expected) in [
        (None, AvailabilityFilter::All),
        (Some("availability=all"), AvailabilityFilter::All),
        (Some("availability="), AvailabilityFilter::All),
        (Some("availability=local"), AvailabilityFilter::Local),
    ] {
        assert_eq!(
            SearchParams::from_query(query).unwrap().availability,
            expected,
            "{query:?}"
        );
    }

    let err = SearchParams::from_query(Some("availability=maybe")).unwrap_err();
    assert!(matches!(&err, SearchError::InvalidAvailability(value) if value == "maybe"));
    assert!(err.is_bad_request());
    assert_eq!(err.to_string(), "invalid availability filter \"maybe\"");

    assert_eq!(AvailabilityFilter::from_value("local"), Some(AvailabilityFilter::Local));
    assert_eq!(AvailabilityFilter::from_value("unknown"), None);
    assert_eq!(
        [AvailabilityFilter::All.as_str(), AvailabilityFilter::Local.as_str()],
        ["all", "local"]
    );
}

/// Two documents differing only in local availability, so the availability filter can be shown to
/// keep the local one and drop the remote-only one while the response echoes the active filter.
struct AvailabilityDocs;

impl PackageIndexer for AvailabilityDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok([("here", true), ("elsewhere", false)]
            .into_iter()
            .map(|(name, available_locally)| PackageDocument {
                display_name: name.to_owned(),
                normalized_name: name.to_owned(),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: PackageSource::Cached,
                available_locally,
                summary: None,
                text: name.to_owned(),
            })
            .collect())
    }
}

#[test]
fn test_availability_filter_keeps_local_and_echoes_the_active_filter() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(AvailabilityDocs));

    let all = search.search(&stores.ctx(&lexicons), SearchParams::default()).unwrap();
    assert_eq!(all.availability, AvailabilityFilter::All);
    assert_eq!(all.total, 2);
    assert!(
        all.results
            .iter()
            .find(|result| result.display_name == "here")
            .unwrap()
            .available_locally
    );
    assert!(
        !all.results
            .iter()
            .find(|result| result.display_name == "elsewhere")
            .unwrap()
            .available_locally
    );

    let local = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                availability: AvailabilityFilter::Local,
                ..SearchParams::default()
            },
        )
        .unwrap();
    assert_eq!(local.availability, AvailabilityFilter::Local);
    assert_eq!(
        (
            local.total,
            local
                .results
                .iter()
                .map(|result| result.display_name.as_str())
                .collect::<Vec<_>>()
        ),
        (1, vec!["here"])
    );
}

struct AccessDocs;

impl PackageIndexer for AccessDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok([("private", "hidden"), ("private", "team/app"), ("public", "visible")]
            .into_iter()
            .map(|(route, name)| PackageDocument {
                display_name: name.to_owned(),
                normalized_name: name.to_owned(),
                route: route.to_owned(),
                index: route.to_owned(),
                ecosystem: "alpha".to_owned(),
                source: PackageSource::Cached,
                available_locally: false,
                summary: None,
                text: name.to_owned(),
            })
            .collect())
    }
}
