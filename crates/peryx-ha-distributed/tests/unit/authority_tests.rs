use crate::authority::{Admission, AuthorityFence, AuthorityKey, CommitOutcome};
use crate::envelope::AuthorityEpoch;

fn key(name: &str) -> AuthorityKey {
    AuthorityKey(name.to_owned())
}

#[test]
fn test_admit_only_at_the_committed_epoch() {
    let mut fence = AuthorityFence::new();
    let home = key("root/alpha/resource-a");

    assert_eq!(fence.commit(&home, AuthorityEpoch(3)), CommitOutcome::Committed);

    assert_eq!(fence.admit(&home, AuthorityEpoch(3)), Admission::Admit);
    assert_eq!(
        fence.admit(&home, AuthorityEpoch(2)),
        Admission::Fenced {
            committed: AuthorityEpoch(3),
            presented: AuthorityEpoch(2),
        }
    );
    assert_eq!(
        fence.admit(&home, AuthorityEpoch(4)),
        Admission::Fenced {
            committed: AuthorityEpoch(3),
            presented: AuthorityEpoch(4),
        }
    );
}

#[test]
fn test_commit_advances_and_refences_the_old_epoch() {
    let mut fence = AuthorityFence::new();
    let home = key("a");
    fence.commit(&home, AuthorityEpoch(3));
    assert_eq!(fence.admit(&home, AuthorityEpoch(3)), Admission::Admit);

    assert_eq!(fence.commit(&home, AuthorityEpoch(4)), CommitOutcome::Committed);

    assert_eq!(
        fence.admit(&home, AuthorityEpoch(3)),
        Admission::Fenced {
            committed: AuthorityEpoch(4),
            presented: AuthorityEpoch(3),
        }
    );
    assert_eq!(fence.admit(&home, AuthorityEpoch(4)), Admission::Admit);
}

#[test]
fn test_duplicate_and_stale_commit_are_ignored() {
    let mut fence = AuthorityFence::new();
    let home = key("a");

    assert_eq!(fence.commit(&home, AuthorityEpoch(3)), CommitOutcome::Committed);
    assert_eq!(fence.commit(&home, AuthorityEpoch(3)), CommitOutcome::Ignored);
    assert_eq!(fence.commit(&home, AuthorityEpoch(2)), CommitOutcome::Ignored);

    assert_eq!(fence.committed_epoch(&home), AuthorityEpoch(3));
}

#[test]
fn test_authorities_fence_independently() {
    let mut fence = AuthorityFence::new();
    let one = key("one");
    let two = key("two");
    fence.commit(&one, AuthorityEpoch(3));
    fence.commit(&two, AuthorityEpoch(7));

    assert_eq!(fence.admit(&one, AuthorityEpoch(3)), Admission::Admit);
    assert_eq!(fence.admit(&two, AuthorityEpoch(7)), Admission::Admit);
    assert_eq!(
        fence.admit(&one, AuthorityEpoch(7)),
        Admission::Fenced {
            committed: AuthorityEpoch(3),
            presented: AuthorityEpoch(7),
        }
    );

    fence.commit(&one, AuthorityEpoch(5));
    assert_eq!(fence.committed_epoch(&two), AuthorityEpoch(7));
    assert_eq!(fence.admit(&two, AuthorityEpoch(7)), Admission::Admit);
}

#[test]
fn test_unassigned_authority_fences_all_work() {
    let fence = AuthorityFence::new();
    let home = key("never-assigned");

    assert_eq!(fence.committed_epoch(&home), AuthorityEpoch(0));
    assert_eq!(
        fence.admit(&home, AuthorityEpoch(1)),
        Admission::Fenced {
            committed: AuthorityEpoch(0),
            presented: AuthorityEpoch(1),
        }
    );
}
