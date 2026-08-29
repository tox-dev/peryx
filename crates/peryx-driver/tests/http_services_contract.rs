use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use peryx_core::Ecosystem;
use peryx_driver::http_services::{
    HttpDomainServices, NewRepository, PolicyDecisionQuery, RepositoryQuery, RepositoryService, RepositoryState,
    RepositoryStateError, RepositoryUpdate, StoreServices, VersionPrecondition,
};
use peryx_driver::retention::{RetentionExport, RetentionQuery};
use peryx_driver::serving::RetentionDriver;
use peryx_driver::trash::{TrashQuery, TrashRef};
use peryx_driver::{AppState, Index, IndexKind};
use peryx_identity::{IndexAcl, UserId};
use peryx_policy::{
    Policy, PolicyAction, PolicyConfig, PolicyDecisionState, RetentionClass, RetentionConfig, RetentionDecision,
    RetentionOutcome, RetentionPolicy, RetentionSummary, RetentionVisibility,
};
use peryx_pql::{PqlError, QueryScope, RepoScope, Value, bind, parse};
use peryx_search::{SearchAccess, SearchParams};
use peryx_storage::blob::{BlobStorage, BlobStore, S3Config, S3Settings};
use peryx_storage::meta::{MetaStore, NewPolicyDecision};
use rstest::rstest;

struct Fixture {
    _dir: tempfile::TempDir,
    state: Arc<AppState>,
}

impl Fixture {
    fn new(indexes: Vec<Index>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        Self::with_blobs(dir, indexes, blobs)
    }

    fn with_blobs(dir: tempfile::TempDir, indexes: Vec<Index>, blobs: impl Into<BlobStorage>) -> Self {
        Self {
            state: Arc::new(AppState::with_clock(
                MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
                blobs,
                60,
                indexes,
                Arc::new(|| 42),
            )),
            _dir: dir,
        }
    }
}

fn s3_storage(conditional_writes: bool, staging: &std::path::Path) -> BlobStorage {
    BlobStorage::s3(
        S3Config::new(S3Settings {
            endpoint: "https://s3.example.com".to_owned(),
            bucket: "bucket".to_owned(),
            prefix: String::new(),
            region: "us-east-1".to_owned(),
            path_style: true,
            request_timeout: Duration::from_secs(5),
            max_retries: 0,
            multipart_threshold: 16 << 20,
            part_size: 8 << 20,
            upload_concurrency: 1,
            conditional_writes,
            checksum_writes: true,
        })
        .unwrap(),
        staging.to_path_buf(),
    )
}

fn index() -> Index {
    Index {
        name: "source".to_owned(),
        route: "root/source".to_owned(),
        ecosystem: Ecosystem::new("neutral"),
        kind: IndexKind::Hosted { volatile: false },
        policy: Policy::compile(&PolicyConfig::default(), str::to_owned),
        acl: IndexAcl {
            anonymous_read: false,
            tokens: Vec::new(),
        },
    }
}

fn repository(actor: UserId) -> NewRepository {
    NewRepository {
        route: "root/source".to_owned(),
        display_name: "Source".to_owned(),
        ecosystem: "neutral".to_owned(),
        definition: serde_json::Value::Null,
        created_by: actor,
    }
}

#[test]
fn repository_service_owns_the_complete_store_lifecycle() {
    let fixture = Fixture::new(Vec::new());
    let actor = UserId::random();
    let service = StoreServices::new(fixture.state.serving.meta.clone());
    let created = service.create(repository(actor.clone()), 1).unwrap();
    let listed = service.list(&RepositoryQuery::default()).unwrap();
    let inspected = service.inspect(&created.id).unwrap().unwrap();
    let updated = service
        .update(
            &created.id,
            VersionPrecondition::exact(1),
            RepositoryUpdate {
                display_name: "Renamed".to_owned(),
                definition: serde_json::json!({"visible": true}),
            },
            &actor,
            2,
        )
        .unwrap();
    let disabled = service
        .set_enabled(&created.id, VersionPrecondition::exact(2), false, &actor, 3)
        .unwrap();

    assert_eq!(listed.repositories, vec![created.clone()]);
    assert_eq!(inspected, created);
    assert_eq!((updated.display_name.as_str(), updated.version), ("Renamed", 2));
    assert_eq!((disabled.state, disabled.version), (RepositoryState::Disabled, 3));
    assert!(matches!(
        service.set_enabled(&disabled.id, VersionPrecondition::exact(2), true, &actor, 4),
        Err(RepositoryStateError::PreconditionFailed { current: Some(3) })
    ));
}

#[tokio::test]
async fn domain_services_read_policy_quota_queries_and_status() {
    let fixture = Fixture::new(vec![index()]);
    let services = HttpDomainServices::for_state(&fixture.state)
        .with_repositories(Arc::new(StoreServices::new(fixture.state.serving.meta.clone())));

    assert!(
        services
            .repositories()
            .list(&RepositoryQuery::default())
            .unwrap()
            .repositories
            .is_empty()
    );
    assert!(
        services
            .policy_decisions()
            .query(&PolicyDecisionQuery::default())
            .unwrap()
            .decisions
            .is_empty()
    );
    let quotas = services
        .quota()
        .summaries(&fixture.state.serving.indexes, 0, 1)
        .unwrap();
    assert_eq!((quotas.len(), quotas[0].repository.as_str()), (1, "root/source"));
    assert_eq!(
        services.quota().repository(&fixture.state.serving.indexes[0]).unwrap(),
        quotas[0]
    );

    for query in ["from policy.decisions", "from usage.reads"] {
        let ast = bind(parse(query).unwrap(), &std::collections::BTreeMap::default()).unwrap();
        let page = services
            .pql()
            .execute(&ast, &QueryScope::new(RepoScope::All, "all".to_owned()), None)
            .unwrap();
        assert!(page.rows.is_empty(), "{query}");
    }

    assert!(services.status().current_serial().is_ok());
    assert!(services.status().blobs_healthy().await);
    assert_eq!(services.status().blob_status().backend, "filesystem");
    assert!(services.trash().query(&TrashQuery::default()).unwrap().items.is_empty());
    assert!(
        services
            .trash()
            .inspect(&TrashRef {
                ecosystem: Ecosystem::new("neutral"),
                repository: "root/source".into(),
                resource: "missing".into(),
                artifact: None,
                digest: None,
            })
            .unwrap()
            .is_none()
    );
}

#[rstest]
#[case::conditional(true, "native")]
#[case::unconditional(false, "unsupported")]
fn status_reports_the_configured_s3_conditional_write_capability(
    #[case] conditional_writes: bool,
    #[case] expected: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let staging = dir.path().join("staging");
    let fixture = Fixture::with_blobs(dir, Vec::new(), s3_storage(conditional_writes, &staging));
    let services = HttpDomainServices::for_state(&fixture.state);

    assert_eq!(services.status().blob_status().conditional_write, expected);
}

#[test]
fn domain_services_apply_search_access() {
    let fixture = Fixture::new(Vec::new());
    let response = HttpDomainServices::for_state(&fixture.state)
        .search()
        .search(SearchParams::default(), Some(&SearchAccess::new(Vec::new())))
        .unwrap();

    assert_eq!((response.total, response.results), (0, Vec::new()));
}

#[test]
fn domain_services_page_and_filter_policy_queries() {
    let fixture = Fixture::new(Vec::new());
    for number in 0..101 {
        let resource = format!("resource-{number}");
        fixture
            .state
            .serving
            .meta
            .record_policy_decision(NewPolicyDecision {
                repository: "source",
                resource: &resource,
                group: None,
                artifact: None,
                source: None,
                action: PolicyAction::Serve,
                state: PolicyDecisionState::Allow,
                rule: None,
                reason: None,
                evaluated_at_unix: number,
                next_eligible_at_unix: None,
            })
            .unwrap();
    }
    let services = HttpDomainServices::for_state(&fixture.state);
    let all = bind(parse("from policy.decisions").unwrap(), &BTreeMap::new()).unwrap();
    let scope = QueryScope::new(RepoScope::All, "all".to_owned());
    let mut cursor = None;
    let mut rows = Vec::new();
    loop {
        let page = services.pql().execute(&all, &scope, cursor.as_deref()).unwrap();
        rows.extend(page.rows);
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    assert_eq!(rows.len(), 101);

    let filtered = bind(
        parse(r#"from policy.decisions where resource == "resource-100" select repository, resource"#).unwrap(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        services
            .pql()
            .execute(
                &filtered,
                &QueryScope::new(
                    RepoScope::Only(BTreeSet::from(["source".to_owned()])),
                    "source".to_owned()
                ),
                None,
            )
            .unwrap()
            .rows,
        [vec![
            Value::Str("source".to_owned()),
            Value::Str("resource-100".to_owned())
        ]]
    );

    let multiple_resources = bind(
        parse(r#"from policy.decisions where resource in ("resource-1", "resource-2") select resource"#).unwrap(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        services
            .pql()
            .execute(
                &multiple_resources,
                &QueryScope::new(RepoScope::All, "all".to_owned()),
                None
            )
            .unwrap()
            .rows,
        [
            vec![Value::Str("resource-2".to_owned())],
            vec![Value::Str("resource-1".to_owned())]
        ]
    );

    let scanned = bind(
        parse(r#"from policy.decisions where state == "allow""#).unwrap(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        services
            .pql()
            .execute(&scanned, &QueryScope::new(RepoScope::All, "all".to_owned()), None)
            .unwrap()
            .rows
            .len(),
        25
    );

    let unknown = bind(parse("from missing.domain").unwrap(), &BTreeMap::new()).unwrap();
    assert!(matches!(
        services
            .pql()
            .execute(&unknown, &QueryScope::new(RepoScope::All, "all".to_owned()), None),
        Err(PqlError::Unauthorized)
    ));
}

#[test]
fn domain_services_preserve_policy_filter_validation() {
    let fixture = Fixture::new(Vec::new());
    let services = HttpDomainServices::for_state(&fixture.state);
    let query = bind(
        parse(&format!(
            "from policy.decisions where resource == \"{}\"",
            "x".repeat(513)
        ))
        .unwrap(),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        services
            .pql()
            .execute(&query, &QueryScope::new(RepoScope::All, "all".to_owned()), None),
        Err(PqlError::Validation("resource filter exceeds 512 bytes".to_owned()))
    );
}

struct OneRetentionDriver;

impl RetentionDriver for OneRetentionDriver {
    fn validate_retention(&self, _policy: &RetentionPolicy) -> Result<(), String> {
        Ok(())
    }

    fn plan_retention(
        &self,
        _meta: &MetaStore,
        _index: &str,
        policy: &RetentionPolicy,
        _now: Option<i64>,
        start: &mut dyn FnMut(RetentionSummary) -> Result<(), String>,
        emit: &mut dyn FnMut(RetentionDecision) -> Result<(), String>,
    ) -> Result<(), String> {
        self.validate_retention(policy)?;
        start(RetentionSummary {
            policy_version: policy.version(),
            frontier: peryx_policy::RetentionFrontier::default(),
        })?;
        emit(retention_decision())?;
        Ok(())
    }
}

fn retention_decision() -> RetentionDecision {
    RetentionDecision {
        resource: "demo".to_owned(),
        group: Some("1.0".to_owned()),
        artifact: "demo.bin".to_owned(),
        digest: "sha-demo".to_owned(),
        class: RetentionClass::Hosted,
        visibility: RetentionVisibility::Active,
        source: None,
        bytes: 10,
        outcome: RetentionOutcome::Remove,
        rule: Some("resource-prefix"),
        retained_groups: Vec::new(),
    }
}

#[tokio::test]
async fn retention_service_owns_store_access_gating_and_export() {
    let fixture = Fixture::new(Vec::new());
    let services = HttpDomainServices::for_state(&fixture.state);
    let policy = RetentionPolicy::compile(&RetentionConfig::default(), str::to_owned);
    let summary = RetentionSummary {
        policy_version: policy.version(),
        frontier: peryx_policy::RetentionFrontier::default(),
    };
    let permit = services.retention().try_enter("source").unwrap();
    let page = services
        .retention()
        .plan(
            &OneRetentionDriver,
            &RetentionQuery {
                index: "source",
                ecosystem: "example",
                policy: &policy,
                now: Some(42),
                after: 0,
                limit: Some(10),
                expect: Some(summary),
            },
            &mut |_| Ok(()),
            &mut |_| Ok(()),
        )
        .unwrap();
    let (_, body) = services
        .retention()
        .export(
            Arc::new(OneRetentionDriver),
            RetentionExport {
                index: "source".to_owned(),
                ecosystem: "example".to_owned(),
                policy,
                now: Some(42),
                after: 0,
                expect: Some(summary),
            },
            permit,
        )
        .await
        .unwrap();
    let exported = axum::body::to_bytes(body, 4096).await.unwrap();

    assert_eq!((page.summary, page.emitted, page.next_cursor), (summary, 1, None));
    assert!(std::str::from_utf8(&exported).unwrap().contains("\"summary\""));
}
