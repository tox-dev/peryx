use std::fmt::Write as _;
use std::hint::black_box;
use std::time::Instant;

use peryx_ecosystem_pypi::catalog::{CatalogSyncOutcome, sync_catalog};
use peryx_ecosystem_pypi::store::catalog_state;
use peryx_index::serving::Inflight;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROJECTS: usize = 1_000_000;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut body = String::with_capacity(PROJECTS * 28);
    body.push_str(r#"{"meta":{"api-version":"1.4"},"projects":["#);
    for position in 0..PROJECTS {
        if position != 0 {
            body.push(',');
        }
        write!(body, r#"{{"name":"package-{position:07}"}}"#)?;
    }
    body.push_str("]}");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "million-projects")
                .set_body_raw(body, "application/vnd.pypi.simple.v1+json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri()))?;
    let directory = tempfile::tempdir()?;
    let meta = MetaStore::open(directory.path().join("peryx.redb"))?;
    meta.put_driver_value("benchmark\0foreground", b"probe")?;

    let started = Instant::now();
    let sync_client = client.clone();
    let sync_meta = meta.clone();
    let sync = tokio::spawn(async move {
        sync_catalog(
            &sync_client,
            &Inflight::default(),
            &sync_meta,
            "benchmark",
            sync_client.base_url(),
        )
        .await
    });
    let mut foreground = Vec::new();
    while !sync.is_finished() {
        let read_started = Instant::now();
        black_box(meta.get_driver_value("benchmark\0foreground")?);
        foreground.push(read_started.elapsed());
        tokio::task::yield_now().await;
    }
    let outcome = sync.await??;
    let elapsed = started.elapsed();
    foreground.sort_unstable();
    let p99 = foreground[(foreground.len() - 1) * 99 / 100];
    let requests = server.received_requests().await.unwrap_or_default().len();
    let projects = catalog_state(&meta, "benchmark")?.active.unwrap().projects;

    assert_eq!(
        outcome,
        CatalogSyncOutcome::Published {
            projects: PROJECTS as u64
        }
    );
    assert_eq!(requests, 1);
    println!("projects={projects}");
    println!("requests={requests}");
    println!("wall_ms={}", elapsed.as_millis());
    println!("foreground_reads={}", foreground.len());
    println!("foreground_p99_us={}", p99.as_micros());
    Ok(())
}
