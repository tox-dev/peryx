use std::sync::Arc;

use peryx_core::{Ecosystem, LexiconRegistry};
use rstest::rstest;

use super::{ALT_WORDS, Stores};
use crate::{
    AvailabilityFilter, ContentSource, INDEXED_TEXT_BYTES, IndexerCtx, SearchAccess, SearchAccessPattern,
    SearchDocument, SearchDocumentProvider, SearchError, SearchIndex, SearchParams, SourceFilter,
};

struct OneDoc {
    name: &'static str,
    ecosystem: &'static str,
}

impl SearchDocumentProvider for OneDoc {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok(vec![SearchDocument {
            display_label: self.name.to_owned(),
            resource_key: self.name.to_owned(),
            route: "root".to_owned(),
            index: "root".to_owned(),
            ecosystem: self.ecosystem.to_owned(),
            source: ContentSource::Cached,
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
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "alpha-resource",
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
        .find(|result| result.display_label == "alpha-resource")
        .unwrap();
    let beta = all
        .results
        .iter()
        .find(|result| result.display_label == "beta-collection")
        .unwrap();
    assert_eq!((&*alpha.type_label, &*beta.type_label), ("resource", "component"));
}

#[test]
fn test_search_rebuilds_after_epoch_bump() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let mut lexicons = LexiconRegistry::default();
    lexicons.register(Ecosystem::new("other"), &ALT_WORDS);
    let mut search = SearchIndex::in_memory();
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
    let mut search = SearchIndex::in_memory();
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

#[rstest]
#[case::empty_pattern("re:", 1)]
#[case::regex_special("++", 0)]
fn test_search_answers_an_empty_pattern_and_a_regex_special_literal(#[case] query: &str, #[case] expected: usize) {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "demo",
        ecosystem: "alpha",
    }));

    assert_eq!(
        search
            .search(
                &stores.ctx(&lexicons),
                SearchParams {
                    query: query.to_owned(),
                    pattern_authority: true,
                    ..SearchParams::default()
                },
            )
            .unwrap()
            .total,
        expected
    );
}

#[test]
fn test_search_rejects_invalid_regex_queries() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let search = SearchIndex::in_memory();

    let error = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: "re:[".to_owned(),
                pattern_authority: true,
                ..SearchParams::default()
            },
        )
        .expect_err("an invalid regular expression should be rejected");

    assert!(error.is_bad_request());
}

#[test]
fn test_search_refuses_a_pattern_without_operator_authority() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "demo",
        ecosystem: "alpha",
    }));

    let error = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: "re:demo".to_owned(),
                ..SearchParams::default()
            },
        )
        .expect_err("a pattern reads every document, so it needs operator authority");

    assert!(error.is_forbidden() && !error.is_bad_request());
}

#[rstest]
#[case::single_character("a")]
#[case::single_character_after_trimming("  b  ")]
fn test_search_refuses_a_query_below_the_ngram_width(#[case] query: &str) {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "demo",
        ecosystem: "alpha",
    }));

    let error = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: query.to_owned(),
                ..SearchParams::default()
            },
        )
        .expect_err("a query with no n-gram names no candidate documents");

    assert!(error.is_bad_request());
}

#[rstest]
#[case::non_digit(r"re:\D+", &["alpha", "release 123", "xreleasey"])]
#[case::digit(r"re:\d+", &["release 123"])]
#[case::non_whitespace(r"re:\S+", &["alpha", "release 123", "xreleasey"])]
#[case::whitespace(r"re:\s+", &["release 123"])]
#[case::uppercase_literal("re:RELEASE", &["release 123", "xreleasey"])]
#[case::grouped_alternation("re:alpha|release", &["alpha", "release 123", "xreleasey"])]
fn test_search_preserves_regex_source(#[case] query: &str, #[case] expected: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    for name in ["alpha", "release 123", "xreleasey"] {
        search.add_indexer(Arc::new(OneDoc {
            name,
            ecosystem: "alpha",
        }));
    }

    let response = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: query.to_owned(),
                pattern_authority: true,
                ..SearchParams::default()
            },
        )
        .unwrap();

    assert_eq!(
        (
            response.total,
            response
                .results
                .iter()
                .map(|result| result.display_label.as_str())
                .collect::<Vec<_>>()
        ),
        (expected.len(), expected.to_vec())
    );
}

#[test]
fn test_search_folds_case_for_non_ascii_text() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "ZÜRICH",
        ecosystem: "alpha",
    }));

    for (case, query) in [
        ("uppercase accented substring term", "ZÜRICH"),
        ("lowercase accented substring term", "zürich"),
        ("uppercase accented regex over the raw field", "re:ZÜRICH"),
        ("accented regex over the raw field", "re:zürich"),
    ] {
        let response = search
            .search(
                &stores.ctx(&lexicons),
                SearchParams {
                    query: query.to_owned(),
                    pattern_authority: true,
                    ..SearchParams::default()
                },
            )
            .unwrap();

        assert_eq!(
            response
                .results
                .iter()
                .map(|result| result.display_label.as_str())
                .collect::<Vec<_>>(),
            ["ZÜRICH"],
            "{case}"
        );
    }
}

struct TextDocs(Vec<(&'static str, String)>);

impl SearchDocumentProvider for TextDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok(self
            .0
            .iter()
            .map(|(name, text)| SearchDocument {
                display_label: (*name).to_owned(),
                resource_key: (*name).to_owned(),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: ContentSource::Cached,
                available_locally: false,
                summary: None,
                text: text.clone(),
            })
            .collect())
    }
}

#[test]
fn test_long_query_verifies_the_full_substring_after_the_ngram_prefilter() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(TextDocs(vec![
        ("separated", "abcdefghijkl xx bcdefghijklm".to_owned()),
        ("whole", "zzabcdefghijklmzz".to_owned()),
    ])));

    let response = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: "abcdefghijklm".to_owned(),
                ..SearchParams::default()
            },
        )
        .unwrap();

    assert_eq!(
        (
            response.total,
            response
                .results
                .iter()
                .map(|result| result.display_label.as_str())
                .collect::<Vec<_>>()
        ),
        (1, vec!["whole"])
    );
}

#[rstest]
#[case::inside_limit(40 * 1024, "abcdefghijklm", 0, &["abcdefghijklm"], &[1])]
#[case::at_the_limit(INDEXED_TEXT_BYTES - "abcdefghijklm".len(), "abcdefghijklm", 0, &["abcdefghijklm"], &[1])]
#[case::after_limit(
    INDEXED_TEXT_BYTES,
    "release-candidate-2026",
    0,
    &["candidate", "release-candidate-2026"],
    &[0, 0]
)]
#[case::cut_through_multibyte(
    INDEXED_TEXT_BYTES - 1,
    "éabcdefghijklm",
    0,
    &["éabcdef", "éabcdefghijklm"],
    &[0, 0]
)]
#[case::multibyte_before_the_cut(
    60 * 1024,
    "éabcdefghijklm",
    4 * 1024,
    &["éabcdef", "éabcdefghijklm"],
    &[1, 1]
)]
fn test_search_bounds_both_query_paths_to_the_indexed_window(
    #[case] prefix_bytes: usize,
    #[case] marker: &str,
    #[case] trailing_chars: usize,
    #[case] queries: &[&str],
    #[case] expected: &[usize],
) {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(TextDocs(vec![(
        "window",
        format!("{}{marker}{}", "x".repeat(prefix_bytes), "é".repeat(trailing_chars)),
    )])));

    assert_eq!(
        queries
            .iter()
            .map(|query| search_total(&search, &stores, &lexicons, query))
            .collect::<Vec<_>>(),
        expected
    );
}

/// The verified clause must survive being the seeked side of an intersection, which happens when a
/// cheaper filter leads it.
#[test]
fn test_a_long_query_intersects_with_a_narrower_filter() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(SourcedDocs));

    let response = search
        .search(
            &stores.ctx(&lexicons),
            SearchParams {
                query: "abcdefghijklm".to_owned(),
                source: SourceFilter::Uploaded,
                ..SearchParams::default()
            },
        )
        .unwrap();

    assert_eq!(
        (
            response.total,
            response
                .results
                .iter()
                .map(|result| result.display_label.as_str())
                .collect::<Vec<_>>()
        ),
        (1, vec!["uploaded"])
    );
}

/// Five documents share the verified substring; one of them carries the narrower source.
struct SourcedDocs;

impl SearchDocumentProvider for SourcedDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok((0..5)
            .map(|position| SearchDocument {
                display_label: if position == 4 {
                    "uploaded".to_owned()
                } else {
                    format!("cached-{position}")
                },
                resource_key: format!("resource-{position}"),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: if position == 4 {
                    ContentSource::Uploaded
                } else {
                    ContentSource::Cached
                },
                available_locally: false,
                summary: None,
                text: "zzabcdefghijklmzz".to_owned(),
            })
            .collect())
    }
}

#[test]
fn test_authorized_search_matches_a_glob_metacharacter_literally() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(TextDocs(vec![
        ("team.app", "team.app".to_owned()),
        ("teamxapp", "teamxapp".to_owned()),
    ])));

    let response = search
        .search_authorized(
            &stores.ctx(&lexicons),
            SearchParams::default(),
            &SearchAccess::new(vec![SearchAccessPattern {
                route: "root".to_owned(),
                glob: "team.app".to_owned(),
            }]),
        )
        .unwrap();

    assert_eq!(
        response
            .results
            .iter()
            .map(|result| result.resource_key.as_str())
            .collect::<Vec<_>>(),
        ["team.app"]
    );
}

fn search_total(search: &SearchIndex, stores: &Stores, lexicons: &LexiconRegistry, query: &str) -> usize {
    search
        .search(
            &stores.ctx(lexicons),
            SearchParams {
                query: query.to_owned(),
                ..SearchParams::default()
            },
        )
        .unwrap()
        .total
}

#[test]
fn test_authorized_search_filters_before_counting_and_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let lexicons = LexiconRegistry::default();
    let mut search = SearchIndex::in_memory();
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
                    .map(|result| result.resource_key.as_str())
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

struct AvailabilityDocs;

impl SearchDocumentProvider for AvailabilityDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok([("here", true), ("elsewhere", false)]
            .into_iter()
            .map(|(name, available_locally)| SearchDocument {
                display_label: name.to_owned(),
                resource_key: name.to_owned(),
                route: "root".to_owned(),
                index: "root".to_owned(),
                ecosystem: "alpha".to_owned(),
                source: ContentSource::Cached,
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
    let mut search = SearchIndex::in_memory();
    search.add_indexer(Arc::new(AvailabilityDocs));

    let all = search.search(&stores.ctx(&lexicons), SearchParams::default()).unwrap();
    assert_eq!(
        (
            all.availability,
            all.total,
            all.results
                .iter()
                .map(|result| (result.display_label.as_str(), result.available_locally))
                .collect::<Vec<_>>(),
        ),
        (AvailabilityFilter::All, 2, vec![("elsewhere", false), ("here", true)])
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
    assert_eq!(
        (
            local.availability,
            local.total,
            local
                .results
                .iter()
                .map(|result| result.display_label.as_str())
                .collect::<Vec<_>>()
        ),
        (AvailabilityFilter::Local, 1, vec!["here"])
    );
}

struct AccessDocs;

impl SearchDocumentProvider for AccessDocs {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        Ok([("private", "hidden"), ("private", "team/app"), ("public", "visible")]
            .into_iter()
            .map(|(route, name)| SearchDocument {
                display_label: name.to_owned(),
                resource_key: name.to_owned(),
                route: route.to_owned(),
                index: route.to_owned(),
                ecosystem: "alpha".to_owned(),
                source: ContentSource::Cached,
                available_locally: false,
                summary: None,
                text: name.to_owned(),
            })
            .collect())
    }
}
