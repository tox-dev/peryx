use std::sync::Arc;

use peryx_core::{Ecosystem, LexiconRegistry};

use super::{OCI_WORDS, Stores};
use crate::context::IndexerCtx;
use crate::{
    PackageDocument, PackageIndexer, PackageSearch, PackageSource, SearchAccess, SearchAccessPattern, SearchError,
    SearchParams,
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
    lexicons.register(Ecosystem::Oci, &OCI_WORDS);
    let mut search = PackageSearch::in_memory();
    search.add_indexer(Arc::new(OneDoc {
        name: "pyalpha",
        ecosystem: "pypi",
    }));
    search.add_indexer(Arc::new(OneDoc {
        name: "ocibeta",
        ecosystem: "oci",
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

    let pypi = all
        .results
        .iter()
        .find(|result| result.display_name == "pyalpha")
        .unwrap();
    let oci = all
        .results
        .iter()
        .find(|result| result.display_name == "ocibeta")
        .unwrap();
    // Each result is labeled in its ecosystem's own word, resolved server-side from the lexicon.
    assert_eq!(pypi.type_label, "package");
    assert_eq!(oci.type_label, "image");
}

#[test]
fn test_search_rebuilds_after_epoch_bump() {
    let dir = tempfile::tempdir().unwrap();
    let stores = Stores::open(&dir);
    let mut lexicons = LexiconRegistry::default();
    lexicons.register(Ecosystem::Oci, &OCI_WORDS);
    let mut search = PackageSearch::in_memory();
    let params = SearchParams::default();
    let before = search.search(&stores.ctx(&lexicons), params.clone()).unwrap().total;

    search.add_indexer(Arc::new(OneDoc {
        name: "ocibeta",
        ecosystem: "oci",
    }));
    search.bump_epoch();

    assert_eq!(
        (before, search.search(&stores.ctx(&lexicons), params).unwrap().total),
        (0, 1)
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
                ecosystem: "pypi".to_owned(),
                source: PackageSource::Cached,
                summary: None,
                text: name.to_owned(),
            })
            .collect())
    }
}
