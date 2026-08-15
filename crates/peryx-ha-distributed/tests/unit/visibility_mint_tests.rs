use peryx_storage::meta::MetaStore;

use crate::envelope::AuthorityEpoch;
use crate::visibility::VisibilityAction::{Lift, Restore, Revoke, Trash};
use crate::visibility::{ApplyEffect, ArtifactId, OpOrder, Visibility, VisibilityOp, VisibilityState};
use crate::visibility_mint::{JournalSerials, SerialSource, StaleEpoch, VisibilityMinter};

#[derive(Debug)]
struct Counter(u64);

impl SerialSource for Counter {
    type Error = std::convert::Infallible;

    fn next_serial(&mut self) -> Result<u64, Self::Error> {
        self.0 += 1;
        Ok(self.0)
    }
}

struct Exhausted;

struct Failing;

impl SerialSource for Failing {
    type Error = Exhausted;

    fn next_serial(&mut self) -> Result<u64, Self::Error> {
        Err(Exhausted)
    }
}

fn artifact(coordinate: &str) -> ArtifactId {
    ArtifactId {
        coordinate: coordinate.to_owned(),
        digest: "sha256:deadbeef".to_owned(),
    }
}

fn minter(epoch: u64) -> VisibilityMinter<Counter> {
    VisibilityMinter::new(AuthorityEpoch(epoch), Counter(0))
}

#[test]
fn test_mint_stamps_the_current_epoch_and_a_fresh_serial() {
    let art = artifact("root/alpha/resource-a/artifact-a");
    let op = minter(4).mint(art.clone(), Trash).unwrap();
    assert_eq!(
        op,
        VisibilityOp {
            artifact: art,
            action: Trash,
            order: OpOrder { epoch: 4, serial: 1 },
        }
    );
}

#[test]
fn test_successive_mints_draw_strictly_increasing_serials() {
    let mut minter = minter(1);
    let art = artifact("root/alpha/resource-a/artifact-a");
    let first = minter.mint(art.clone(), Trash).unwrap();
    let second = minter.mint(art, Revoke).unwrap();
    assert_eq!(first.order, OpOrder { epoch: 1, serial: 1 });
    assert_eq!(second.order, OpOrder { epoch: 1, serial: 2 });
}

#[test]
fn test_an_equal_order_pair_of_different_actions_cannot_be_minted() {
    let mut minter = minter(1);
    let art = artifact("root/alpha/resource-a/artifact-a");
    let trash = minter.mint(art.clone(), Trash).unwrap();
    let restore = minter.mint(art, Restore).unwrap();
    assert_ne!(trash.order, restore.order);
}

#[test]
fn test_every_minted_order_is_unique_across_artifacts_and_actions() {
    let mut minter = minter(7);
    let actions = [Trash, Restore, Revoke, Lift];
    let mut orders = std::collections::BTreeSet::new();
    for (index, action) in actions.into_iter().enumerate() {
        let op = minter
            .mint(artifact(&format!("root/alpha/resource/{index}")), action)
            .unwrap();
        assert!(orders.insert(op.order), "order {:?} was minted twice", op.order);
    }
    assert_eq!(orders.len(), actions.len());
}

#[test]
fn test_minted_ops_converge_regardless_of_arrival_order() {
    let mut minter = minter(1);
    let art = artifact("root/alpha/resource-a/artifact-a");
    let ops = [
        minter.mint(art.clone(), Trash).unwrap(),
        minter.mint(art.clone(), Restore).unwrap(),
        minter.mint(art.clone(), Revoke).unwrap(),
    ];

    let apply_all = |sequence: &[&VisibilityOp]| {
        let mut state = VisibilityState::new();
        for op in sequence {
            state.apply(op);
        }
        state.get(&art)
    };

    let in_order = apply_all(&[&ops[0], &ops[1], &ops[2]]);
    let reversed = apply_all(&[&ops[2], &ops[1], &ops[0]]);
    let shuffled = apply_all(&[&ops[1], &ops[2], &ops[0]]);

    assert_eq!(in_order, reversed);
    assert_eq!(shuffled, reversed);
    assert_eq!(
        in_order,
        Visibility {
            trashed: false,
            revoked: true,
        }
    );
}

#[test]
fn test_adopt_epoch_advances_the_stamped_epoch() {
    let mut minter = minter(1);
    minter.adopt_epoch(AuthorityEpoch(2)).unwrap();
    assert_eq!(minter.epoch(), AuthorityEpoch(2));
    let op = minter
        .mint(artifact("root/alpha/resource-a/artifact-a"), Restore)
        .unwrap();
    assert_eq!(op.order.epoch, 2);
}

#[test]
fn test_a_higher_epoch_mint_outranks_a_prior_epoch_op() {
    let mut minter = minter(1);
    let art = artifact("root/alpha/resource-a/artifact-a");
    let trashed = minter.mint(art.clone(), Trash).unwrap();
    minter.adopt_epoch(AuthorityEpoch(2)).unwrap();
    let restored = minter.mint(art.clone(), Restore).unwrap();

    let mut state = VisibilityState::new();
    assert_eq!(state.apply(&restored), ApplyEffect::Applied);
    assert_eq!(state.apply(&trashed), ApplyEffect::Ignored);
    assert!(state.get(&art).is_visible());
}

#[test]
fn test_adopt_epoch_rejects_an_equal_epoch() {
    let mut minter = minter(3);
    let error = minter.adopt_epoch(AuthorityEpoch(3)).unwrap_err();
    assert_eq!((error.current, error.presented), (3, 3));
    assert_eq!(minter.epoch(), AuthorityEpoch(3));
}

#[test]
fn test_adopt_epoch_rejects_a_lower_epoch() {
    let mut minter = minter(3);
    let error = minter.adopt_epoch(AuthorityEpoch(2)).unwrap_err();
    assert_eq!((error.current, error.presented), (3, 2));
    assert_eq!(minter.epoch(), AuthorityEpoch(3));
}

#[test]
fn test_stale_epoch_displays_the_current_and_presented_epochs() {
    let error = StaleEpoch {
        current: 5,
        presented: 4,
    };
    assert_eq!(
        error.to_string(),
        "epoch 4 does not advance the minter's current epoch 5"
    );
}

#[test]
fn test_mint_surfaces_a_serial_source_error() {
    let mut minter = VisibilityMinter::new(AuthorityEpoch(1), Failing);
    assert!(matches!(
        minter.mint(artifact("root/alpha/resource-a/artifact-a"), Trash),
        Err(Exhausted)
    ));
}

#[test]
fn test_journal_serials_draw_strictly_increasing_serials_across_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let art = artifact("root/alpha/resource-a/artifact-a");

    let (trash, restore) = {
        let store = MetaStore::open(&path).unwrap();
        let mut minter = VisibilityMinter::new(AuthorityEpoch(1), JournalSerials::new(&store));
        (
            minter.mint(art.clone(), Trash).unwrap(),
            minter.mint(art.clone(), Restore).unwrap(),
        )
    };
    assert_eq!(trash.order, OpOrder { epoch: 1, serial: 1 });
    assert_eq!(restore.order, OpOrder { epoch: 1, serial: 2 });

    let store = MetaStore::open_existing(&path).unwrap();
    let serials = JournalSerials::new(&store);
    let mut minter = VisibilityMinter::new(AuthorityEpoch(1), serials);
    let after_reopen = minter.mint(art, Revoke).unwrap();
    assert_eq!(after_reopen.order, OpOrder { epoch: 1, serial: 3 });
}
