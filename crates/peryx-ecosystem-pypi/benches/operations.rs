//! Per-operation benchmarks for the `PyPI` driver: end-to-end serving plus the parse, render, and
//! name/version primitives every request is built from. Small and large fixtures bracket the range
//! from a one-file project to a several-hundred-file project like `boto3` or `torch`.
#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary the nursery lint flags"
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use http::Request;
use http_body_util::BodyExt as _;
use peryx_driver::AppState;
use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};
use peryx_ecosystem_pypi::store::CachedIndex;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::{
    CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Provenance, Yanked, normalize_name,
    parse_detail, parse_detail_html, parse_distribution_filename, parse_index, parse_index_html, parse_metadata,
    parse_version, parse_version_specifiers, render_detail_html, render_index_html, render_legacy_json, sorted_desc,
    to_json,
};
use peryx_http::router;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use tokio::runtime::Runtime;
use tower::ServiceExt as _;
use url::Url;

const SMALL: usize = 3;
const LARGE: usize = 400;

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn sample_file(project: &str, index: usize) -> File {
    let version = format!("{}.{}.{}", index / 100, (index / 10) % 10, index % 10);
    let py = 8 + index % 5;
    let filename = format!("{project}-{version}-cp3{py}-cp3{py}-manylinux_2_17_x86_64.whl");
    let mut hashes = BTreeMap::new();
    hashes.insert(
        "sha256".to_owned(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_owned(),
    );
    File {
        url: format!("https://files.pythonhosted.org/packages/ab/cd/{filename}"),
        filename,
        hashes,
        requires_python: Some(">=3.8".to_owned()),
        size: Some(1_000_000 + index as u64),
        upload_time: Some("2024-01-01T00:00:00.000000Z".to_owned()),
        yanked: Yanked::No,
        core_metadata: CoreMetadata::Available,
        dist_info_metadata: CoreMetadata::Absent,
        gpg_sig: None,
        provenance: Provenance::default(),
    }
}

fn project_detail(project: &str, files: usize) -> ProjectDetail {
    let files: Vec<File> = (0..files).map(|index| sample_file(project, index)).collect();
    let mut versions: Vec<String> = files
        .iter()
        .filter_map(|file| file.filename.split('-').nth(1).map(str::to_owned))
        .collect();
    versions.sort();
    versions.dedup();
    ProjectDetail {
        meta: Meta::default(),
        name: project.to_owned(),
        versions,
        files,
    }
}

fn index_list(projects: usize) -> ProjectList {
    ProjectList {
        meta: Meta::default(),
        projects: (0..projects)
            .map(|index| ProjectListEntry {
                name: format!("project-{index}"),
            })
            .collect(),
    }
}

const METADATA: &str = "Metadata-Version: 2.1\n\
Name: flask\n\
Version: 3.0.0\n\
Summary: A simple framework for building complex web applications.\n\
Author-email: Contact <contact@example.com>\n\
Requires-Python: >=3.8\n\
Requires-Dist: Werkzeug>=3.0.0\n\
Requires-Dist: Jinja2>=3.1.2\n\
Requires-Dist: itsdangerous>=2.1.2\n\
Requires-Dist: click>=8.1.3\n\
Requires-Dist: blinker>=1.6.2\n\
Provides-Extra: async\n\
Provides-Extra: dotenv\n\
Description-Content-Type: text/markdown\n\
\n\
# Flask\n\nA simple framework.\n";

fn cached(rate_limit: RateLimitConfig, detail: &ProjectDetail) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_index(
        &format!("pypi/{}", detail.name),
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 1000,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: Some(3600),
            body: to_json(detail).into_bytes(),
        },
    )
    .unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:9/simple/").unwrap();
    let mut state = AppState::with_limits(
        meta,
        blobs,
        3600,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
        Arc::new(|| 1000),
        rate_limit,
        [("pypi".to_owned(), 0)],
    );
    peryx_ecosystem_pypi::install(&mut state);
    (dir, Arc::new(state))
}

const fn enabled_limits() -> RateLimitConfig {
    RateLimitConfig {
        listing: RouteLimit::new(u64::MAX, 60),
        ..RateLimitConfig::enabled_defaults()
    }
}

async fn serve(app: axum::Router, uri: &str, accept: &str) {
    let request = Request::builder()
        .uri(uri)
        .header("accept", accept)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let _ = response.into_body().collect().await.unwrap().to_bytes();
}

const JSON: &str = "application/vnd.pypi.simple.v1+json";
const HTML: &str = "text/html";

/// End-to-end warm serving. The router is built once, as the server does, and the cheap Arc-backed
/// service is cloned per request; rebuilding the route tree each iteration would drown the serve path.
fn bench_serve(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("serve");
    let detail = project_detail("flask", LARGE);
    for (name, rate_limit) in [("disabled", RateLimitConfig::default()), ("enabled", enabled_limits())] {
        let (_dir, state) = cached(rate_limit, &detail);
        let app = router(state);
        rt.block_on(serve(app.clone(), "/pypi/simple/flask/", JSON));
        group.bench_with_input(BenchmarkId::new("simple_json", name), &app, |b, app| {
            b.to_async(&rt).iter(|| serve(app.clone(), "/pypi/simple/flask/", JSON));
        });
        group.bench_with_input(BenchmarkId::new("simple_html", name), &app, |b, app| {
            b.to_async(&rt).iter(|| serve(app.clone(), "/pypi/simple/flask/", HTML));
        });
        group.bench_with_input(BenchmarkId::new("legacy_json", name), &app, |b, app| {
            b.to_async(&rt).iter(|| serve(app.clone(), "/pypi/flask/json", JSON));
        });
    }
    group.finish();
}

fn bench_parse(c: &mut Criterion) {
    let base = Url::parse("https://pypi.org/simple/flask/").unwrap();
    let mut group = c.benchmark_group("parse");
    for (label, files) in [("small", SMALL), ("large", LARGE)] {
        let detail = project_detail("flask", files);
        let json = to_json(&detail).into_bytes();
        let html = render_detail_html(&detail);
        group.bench_function(BenchmarkId::new("detail_json", label), |b| {
            b.iter(|| parse_detail(std::hint::black_box(&json)).unwrap());
        });
        group.bench_function(BenchmarkId::new("detail_html", label), |b| {
            b.iter(|| parse_detail_html("flask", std::hint::black_box(&html), &base).unwrap());
        });
    }
    let list = index_list(LARGE);
    let index_json = to_json(&list).into_bytes();
    let index_html = render_index_html(&list);
    group.bench_function("index_json", |b| {
        b.iter(|| parse_index(std::hint::black_box(&index_json)).unwrap());
    });
    group.bench_function("index_html", |b| {
        b.iter(|| parse_index_html(std::hint::black_box(&index_html), &base).unwrap());
    });
    group.bench_function("metadata", |b| {
        b.iter(|| parse_metadata(std::hint::black_box(METADATA)));
    });
    group.bench_function("distribution_filename", |b| {
        b.iter(|| {
            parse_distribution_filename(std::hint::black_box(
                "flask-3.0.0-cp312-cp312-manylinux_2_17_x86_64.whl",
            ))
        });
    });
    group.finish();
}

fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("render");
    for (label, files) in [("small", SMALL), ("large", LARGE)] {
        let detail = project_detail("flask", files);
        group.bench_function(BenchmarkId::new("to_json", label), |b| {
            b.iter(|| to_json(std::hint::black_box(&detail)));
        });
        group.bench_function(BenchmarkId::new("detail_html", label), |b| {
            b.iter(|| render_detail_html(std::hint::black_box(&detail)));
        });
        group.bench_function(BenchmarkId::new("legacy_json", label), |b| {
            b.iter(|| render_legacy_json(std::hint::black_box(&detail), None));
        });
    }
    let list = index_list(LARGE);
    group.bench_function("index_html", |b| {
        b.iter(|| render_index_html(std::hint::black_box(&list)));
    });
    group.finish();
}

fn bench_name_version(c: &mut Criterion) {
    let mut group = c.benchmark_group("name_version");
    group.bench_function("normalize_name", |b| {
        b.iter(|| normalize_name(std::hint::black_box("Flask.Extension_Name")));
    });
    group.bench_function("parse_version", |b| {
        b.iter(|| parse_version(std::hint::black_box("3.0.1.post2")));
    });
    group.bench_function("parse_version_specifiers", |b| {
        b.iter(|| parse_version_specifiers(std::hint::black_box(">=3.8,<4.0,!=3.9.1")));
    });
    let versions: Vec<String> = (0..LARGE)
        .map(|index| format!("{}.{}.{}", index / 100, (index / 10) % 10, index % 10))
        .collect();
    group.bench_function("sorted_desc", |b| {
        b.iter(|| sorted_desc(std::hint::black_box(&versions)));
    });
    group.finish();
}

criterion_group!(benches, bench_serve, bench_parse, bench_render, bench_name_version);
criterion_main!(benches);
