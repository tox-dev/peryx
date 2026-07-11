use std::sync::Arc;

use peryx_core::{Ecosystem, LexiconRegistry};

use super::{OCI_WORDS, Stores};
use crate::context::IndexerCtx;
use crate::{PackageDocument, PackageIndexer, PackageSearch, PackageSource, SearchError, SearchParams};

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
