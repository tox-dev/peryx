//! What an absent placement row means, pinned at the rule both the file view and search resolve
//! through. A future change that gives either view its own reading has to break one of these.

use peryx_ha::{ArtifactPlacement, ArtifactSource, ByteAvailability};
use rstest::rstest;

use super::resolve_file_placement;

fn placement(source: ArtifactSource, availability: ByteAvailability) -> ArtifactPlacement {
    ArtifactPlacement { source, availability }
}

/// The upload record establishes that this node wrote the bytes, so the projection may only demote
/// that. An absent row demotes nothing, which is what keeps a hosted upload readable while
/// [#2141](https://github.com/tox-dev/peryx/issues/2141) leaves the upload path recording no row.
#[rstest]
#[case::no_row(None, ByteAvailability::Local)]
#[case::hosted_row_local(
    Some(placement(ArtifactSource::Hosted, ByteAvailability::Local)),
    ByteAvailability::Local
)]
#[case::hosted_row_evicted(
    Some(placement(ArtifactSource::Hosted, ByteAvailability::Unavailable)),
    ByteAvailability::Unavailable
)]
#[case::mirror_row_is_not_about_this_upload(
    Some(placement(ArtifactSource::Proxy, ByteAvailability::RemoteOnly)),
    ByteAvailability::Local
)]
fn test_a_hosted_file_reads_local_until_its_own_row_demotes_it(
    #[case] row: Option<ArtifactPlacement>,
    #[case] expected: ByteAvailability,
) {
    assert_eq!(
        resolve_file_placement(true, row),
        ArtifactPlacement {
            source: ArtifactSource::Hosted,
            availability: expected,
        }
    );
}

/// Nothing else says this node holds a proxied file, so the projection has to establish it. An absent
/// row leaves the file remote rather than claiming bytes no read can serve.
#[rstest]
#[case::no_row(None, ArtifactSource::Proxy, ByteAvailability::RemoteOnly)]
#[case::fetched(
    Some(placement(ArtifactSource::Proxy, ByteAvailability::Local)),
    ArtifactSource::Proxy,
    ByteAvailability::Local
)]
#[case::never_fetched(
    Some(placement(ArtifactSource::Proxy, ByteAvailability::RemoteOnly)),
    ArtifactSource::Proxy,
    ByteAvailability::RemoteOnly
)]
#[case::generated(
    Some(placement(ArtifactSource::Generated, ByteAvailability::Local)),
    ArtifactSource::Generated,
    ByteAvailability::Local
)]
fn test_a_proxied_file_reads_remote_until_a_row_says_otherwise(
    #[case] row: Option<ArtifactPlacement>,
    #[case] source: ArtifactSource,
    #[case] availability: ByteAvailability,
) {
    assert_eq!(
        resolve_file_placement(false, row),
        ArtifactPlacement { source, availability }
    );
}

/// An orphan purge deletes the row before the bytes, so a digest can sit with surviving bytes and no
/// row. A hosted upload keeps reading local across that window, which is the answer that matches the
/// bytes still on disk.
#[test]
fn test_the_reclaim_window_leaves_a_hosted_upload_readable() {
    assert_eq!(resolve_file_placement(true, None).availability, ByteAvailability::Local);
}
