use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;

use super::Stores;

#[test]
fn test_indexer_context_resolves_layer_positions() {
    let dir = tempfile::tempdir().unwrap();
    let mut stores = Stores::open(&dir);
    stores.indexes.push(Index {
        name: "hosted".to_owned(),
        route: "hosted".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    });

    assert_eq!(stores.indexer_ctx().index_at(0).name, "hosted");
}
