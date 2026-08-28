use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::future::join_all;
use tokio::sync::Barrier;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::{
    Auth, CredentialError, CredentialFailure, CredentialProvider, CredentialRefresh, UpstreamClient, UpstreamTls,
};

fn refresh(failure: CredentialFailure) -> CredentialRefresh {
    CredentialRefresh {
        interval: Duration::from_mins(1),
        on_unauthorized: true,
        failure,
    }
}

fn unavailable_credential() -> std::future::Ready<Result<Auth, CredentialError>> {
    std::future::ready(Err(CredentialError::new("source unavailable")))
}

#[test]
fn test_fixed_provider_keeps_one_generation() {
    let provider = CredentialProvider::fixed(Auth::Bearer("token".to_owned()));
    let snapshot = provider.current().unwrap();

    assert_eq!(
        (snapshot.auth(), snapshot.generation()),
        (&Auth::Bearer("token".to_owned()), 0)
    );
}

#[test]
fn test_provider_debug_redacts_credentials() {
    let provider = CredentialProvider::fixed(Auth::Bearer("secret-token".to_owned()));

    let debug = format!("{provider:?}");

    assert!(debug.contains("refresh: None"));
    assert!(!debug.contains("secret-token"));
}

#[test]
fn test_clones_share_a_provider_identity() {
    let provider = CredentialProvider::fixed(Auth::None);
    let clone = provider.clone();

    assert_eq!(
        provider.current().unwrap().identity().provider(),
        clone.current().unwrap().identity().provider()
    );
}

#[tokio::test]
async fn test_provider_refreshes_once_for_one_rejected_generation() {
    let loads = Arc::new(AtomicUsize::new(0));
    let provider = CredentialProvider::refreshing(Auth::Bearer("old".to_owned()), refresh(CredentialFailure::Fail), {
        let loads = loads.clone();
        move || {
            let loads = loads.clone();
            async move {
                loads.fetch_add(1, Ordering::Relaxed);
                Ok(Auth::Bearer("new".to_owned()))
            }
        }
    });
    let barrier = Arc::new(Barrier::new(51));
    let requests = join_all((0..50).map(|_| {
        let barrier = barrier.clone();
        let provider = &provider;
        async move {
            barrier.wait().await;
            provider.refresh_after_unauthorized(0).await
        }
    }));

    let (_, snapshots) = tokio::join!(barrier.wait(), requests);

    assert_eq!(loads.load(Ordering::Relaxed), 1);
    assert!(snapshots.into_iter().all(|snapshot| {
        let snapshot = snapshot.unwrap();
        snapshot.generation() == 1 && snapshot.auth() == &Auth::Bearer("new".to_owned())
    }));
}

#[tokio::test]
async fn test_provider_refreshes_after_its_deadline() {
    let provider = CredentialProvider::refreshing(
        Auth::Bearer("old".to_owned()),
        CredentialRefresh {
            interval: Duration::ZERO,
            on_unauthorized: false,
            failure: CredentialFailure::Fail,
        },
        || async { Ok(Auth::Bearer("new".to_owned())) },
    );

    let snapshot = provider.credential().await.unwrap();

    assert_eq!(
        (snapshot.auth(), snapshot.generation()),
        (&Auth::Bearer("new".to_owned()), 1)
    );
}

#[tokio::test]
async fn test_provider_keeps_its_snapshot_before_the_deadline() {
    let provider = CredentialProvider::refreshing(
        Auth::Bearer("old".to_owned()),
        refresh(CredentialFailure::Fail),
        unavailable_credential,
    );

    let snapshot = provider.credential().await.unwrap();

    assert_eq!(
        (snapshot.auth(), snapshot.generation()),
        (&Auth::Bearer("old".to_owned()), 0)
    );
}

#[tokio::test]
async fn test_provider_fail_policy_caches_a_redacted_error() {
    let loads = Arc::new(AtomicUsize::new(0));
    let provider = CredentialProvider::refreshing(Auth::Bearer("old".to_owned()), refresh(CredentialFailure::Fail), {
        let loads = loads.clone();
        move || {
            loads.fetch_add(1, Ordering::Relaxed);
            async { Err(CredentialError::new("secret file /run/keys/token is empty")) }
        }
    });

    let first = provider.refresh_after_unauthorized(0).await.unwrap_err();
    let second = provider.refresh_after_unauthorized(0).await.unwrap_err();

    assert_eq!(first, CredentialError::new("secret file /run/keys/token is empty"));
    assert_eq!(second, first);
    assert_eq!(loads.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn test_provider_anonymous_policy_drops_the_rejected_credential() {
    let provider = CredentialProvider::refreshing(
        Auth::Bearer("old".to_owned()),
        refresh(CredentialFailure::Anonymous),
        unavailable_credential,
    );

    let snapshot = provider.refresh_after_unauthorized(0).await.unwrap();

    assert_eq!((snapshot.auth(), snapshot.generation()), (&Auth::None, 1));
}

#[tokio::test]
async fn test_provider_can_disable_unauthorized_refresh() {
    let provider = CredentialProvider::refreshing(
        Auth::Bearer("old".to_owned()),
        CredentialRefresh {
            interval: Duration::from_mins(1),
            on_unauthorized: false,
            failure: CredentialFailure::Fail,
        },
        unavailable_credential,
    );

    let snapshot = provider.refresh_after_unauthorized(0).await.unwrap();

    assert_eq!(
        (snapshot.auth(), snapshot.generation()),
        (&Auth::Bearer("old".to_owned()), 0)
    );
}

#[tokio::test]
async fn test_client_replays_a_401_with_the_refreshed_generation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/resource/"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/resource/"))
        .and(header("authorization", "Bearer new"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::with_credentials_and_tls_for_origin(
        &format!("{}/api/", server.uri()),
        CredentialProvider::refreshing(
            Auth::Bearer("old".to_owned()),
            refresh(CredentialFailure::Fail),
            || async { Ok(Auth::Bearer("new".to_owned())) },
        ),
        &UpstreamTls::default(),
        &format!("{}/api/", server.uri()),
        &[],
    )
    .unwrap();

    let response = client
        .send_conditional(client.base().join("resource/").unwrap(), "application/json", None)
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn test_client_returns_a_401_when_refresh_is_disabled() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/resource/"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/api/", server.uri())).unwrap();

    let response = client
        .send_conditional(client.base().join("resource/").unwrap(), "application/json", None)
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_client_replays_only_once_after_a_401() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/resource/"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/resource/"))
        .and(header("authorization", "Bearer new"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::with_credentials_and_tls_for_origin(
        &format!("{}/api/", server.uri()),
        CredentialProvider::refreshing(
            Auth::Bearer("old".to_owned()),
            refresh(CredentialFailure::Fail),
            || async { Ok(Auth::Bearer("new".to_owned())) },
        ),
        &UpstreamTls::default(),
        &format!("{}/api/", server.uri()),
        &[],
    )
    .unwrap();

    let response = client
        .send_conditional(client.base().join("resource/").unwrap(), "application/json", None)
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_warm_records_a_credential_refresh_failure() {
    let client = UpstreamClient::with_credentials_and_tls_for_origin(
        "https://example.invalid/api/",
        CredentialProvider::refreshing(
            Auth::Bearer("old".to_owned()),
            CredentialRefresh {
                interval: Duration::ZERO,
                on_unauthorized: true,
                failure: CredentialFailure::Fail,
            },
            || async { Err(CredentialError::new("source unavailable")) },
        ),
        &UpstreamTls::default(),
        "https://example.invalid/api/",
        &[],
    )
    .unwrap();

    client.warm().await;

    assert_eq!(client.reachability(), crate::Reachability::Unreachable);
}
