use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use peryx_driver::{
    AppState, ServingState, ServingStateAvailabilityAuthorizer, ServingStateControlAuthorizer,
    ServingStateMetadataFrontierProvider,
};
use peryx_ha::{
    AvailabilityAudience, AvailabilityAuthorizer as _, ControlAuthorizer as _, ControlPermission,
    MetadataFrontierProvider as _,
};
use peryx_identity::{GrantScope, Role};
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

const PASSWORD: &str = "availability adapter password";

fn state() -> (tempfile::TempDir, Arc<ServingState>) {
    let dir = tempfile::tempdir().unwrap();
    let state = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    );
    (dir, state.serving)
}

async fn principal(state: &ServingState, name: &str, role: Role) -> (String, String) {
    let user = state.users.create(name).unwrap();
    state.users.set_password(&user.id, PASSWORD).await.unwrap();
    state.authorization.grant(&user.id, role, GrantScope::Server).unwrap();
    (
        user.id.to_string(),
        format!("Basic {}", STANDARD.encode(format!("{name}:{PASSWORD}"))),
    )
}

#[rstest]
#[case::missing(None, AvailabilityAudience::Public)]
#[case::malformed(Some("Bearer token"), AvailabilityAudience::Public)]
#[tokio::test]
async fn availability_authorizer_rejects_missing_or_malformed_credentials(
    #[case] authorization: Option<&str>,
    #[case] expected: AvailabilityAudience,
) {
    let (_dir, state) = state();

    assert_eq!(
        ServingStateAvailabilityAuthorizer::new(state)
            .authorize(authorization)
            .await
            .unwrap(),
        expected
    );
}

#[tokio::test]
async fn availability_authorizer_rejects_unknown_credentials() {
    let (_dir, state) = state();
    let authorization = format!("Basic {}", STANDARD.encode(format!("Unknown:{PASSWORD}")));

    assert_eq!(
        ServingStateAvailabilityAuthorizer::new(state)
            .authorize(Some(&authorization))
            .await
            .unwrap(),
        AvailabilityAudience::Public
    );
}

#[rstest]
#[case::reader(Role::RepositoryReader, AvailabilityAudience::Public)]
#[case::operator(Role::Operator, AvailabilityAudience::Operator)]
#[case::administrator(Role::Administrator, AvailabilityAudience::Administrator)]
#[tokio::test]
async fn availability_authorizer_maps_server_roles(#[case] role: Role, #[case] expected: AvailabilityAudience) {
    let (_dir, state) = state();
    let (_, authorization) = principal(&state, "actor", role).await;

    assert_eq!(
        ServingStateAvailabilityAuthorizer::new(state)
            .authorize(Some(&authorization))
            .await
            .unwrap(),
        expected
    );
}

#[tokio::test]
async fn control_authorizer_authenticates_and_checks_permissions() {
    let (_dir, state) = state();
    let (user_id, authorization) = principal(&state, "administrator", Role::Administrator).await;
    let authorizer = ServingStateControlAuthorizer::new(state);

    assert_eq!(authorizer.authenticate(None).await.unwrap(), None);
    assert_eq!(authorizer.authenticate(Some("Basic invalid")).await.unwrap(), None);
    let actor = authorizer.authenticate(Some(&authorization)).await.unwrap().unwrap();
    assert_eq!(actor.as_str(), user_id);
    assert!(authorizer.allows(&actor, ControlPermission::Read));
    assert!(authorizer.allows(&actor, ControlPermission::Write));
}

#[tokio::test]
async fn control_authorizer_denies_an_unprivileged_actor() {
    let (_dir, state) = state();
    let (user_id, _) = principal(&state, "reader", Role::RepositoryReader).await;
    let authorizer = ServingStateControlAuthorizer::new(state);
    let actor = peryx_ha::ControlActor::new(user_id);

    assert!(!authorizer.allows(&actor, ControlPermission::Read));
    assert!(!authorizer.allows(&actor, ControlPermission::Write));
}

#[tokio::test]
async fn metadata_frontier_reports_the_committed_position() {
    let (_dir, state) = state();
    state
        .meta
        .commit_driver_txn::<(), peryx_storage::meta::MetaError>(|transaction| {
            transaction.put("resource", b"value")?;
            Ok(((), vec![b"event".to_vec()]))
        })
        .unwrap();

    assert_eq!(
        ServingStateMetadataFrontierProvider::new(state)
            .frontier("resource")
            .await
            .unwrap(),
        peryx_ha::FrontierReply {
            epoch: 0,
            applied_frontier: 1,
        }
    );
}
