use std::sync::Arc;

use velodex_format::Ecosystem;

use super::{OCI_WORDS, state};
use crate::search::{PackageDocument, PackageIndexer, PackageSource, SearchError, SearchParams};
use crate::state::AppState;

/// A stand-in ecosystem indexer that yields one document of a given ecosystem regardless of state.
struct OneDoc {
    name: &'static str,
    ecosystem: &'static str,
}

impl PackageIndexer for OneDoc {
    fn documents(&self, _state: &AppState) -> Result<Vec<PackageDocument>, SearchError> {
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
    let mut state = state(&dir);
    state.register_lexicon(Ecosystem::Oci, &OCI_WORDS);
    state.add_search_indexer(Arc::new(OneDoc {
        name: "pyalpha",
        ecosystem: "pypi",
    }));
    state.add_search_indexer(Arc::new(OneDoc {
        name: "ocibeta",
        ecosystem: "oci",
    }));

    let all = state
        .search
        .search(
            &state,
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
