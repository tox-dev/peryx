use std::collections::BTreeMap;

use super::*;
use crate::config::{
    AcmeConfig, AvailabilityConfig, AvailabilityListenerConfig, AvailabilityListenerTls, DcMember, DcMembership,
    DcRole, IndexKind, PrefetchConfig, ReplicationConfig, SecretSource, TlsConfig, UpstreamConfig,
    UpstreamRoutingConfig, UpstreamTlsConfig, WebhookConfig, WebhookSecret,
};
use crate::tests::support::{plugins, plugins_without_retention};

fn cancelled() -> tokio_util::sync::CancellationToken {
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();
    cancellation
}

fn local_config(directory: &tempfile::TempDir, plugins: &peryx_plugin_registry::PluginRegistry) -> Config {
    Config {
        host: "127.0.0.1".to_owned(),
        port: 0,
        data_dir: directory.path().to_path_buf(),
        ..Config::with_plugins(plugins)
    }
}

fn certificate_files(directory: &tempfile::TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let key = rcgen::KeyPair::generate().unwrap();
    let certificate = rcgen::CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let cert = directory.path().join("certificate.pem");
    let key_path = directory.path().join("key.pem");
    std::fs::write(&cert, certificate.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();
    (cert, key_path)
}

async fn tls_config(directory: &tempfile::TempDir) -> axum_server::tls_rustls::RustlsConfig {
    let (cert, key) = certificate_files(directory);
    load_tls_config(&cert, &key).await.unwrap()
}

fn make_service() -> MakeService {
    axum::Router::new()
        .route("/ready", axum::routing::get(|| async { "ready" }))
        .into_make_service_with_connect_info::<std::net::SocketAddr>()
}

#[test]
fn test_logging_layers_cover_formats_and_platform_sinks() {
    drop(fmt_layer(LogFormat::Pretty, std::io::sink));
    drop(fmt_layer(LogFormat::Json, std::io::sink));
    let _ = journald_layer(LogFormat::Pretty);
    let _ = syslog_layer(LogFormat::Pretty);
    let _ = syslog_layer(LogFormat::Json);
    for sink in [LogSink::Journald, LogSink::Syslog] {
        let _ = logging_layer(&LogConfig {
            sink,
            ..LogConfig::default()
        });
    }
}

#[tokio::test]
async fn test_process_tasks_propagate_cache_warming_failure() {
    let mut tasks = ProcessTasks::new(tokio_util::sync::CancellationToken::new());
    let (started, ready) = tokio::sync::oneshot::channel();
    tasks.spawn_cache_warming(async move {
        started.send(()).unwrap();
        anyhow::bail!("warm failed")
    });
    ready.await.unwrap();

    assert!(tasks.shutdown().await.unwrap_err().to_string().contains("warm failed"));
}

#[tokio::test]
async fn test_finish_process_combines_owner_failures() {
    let error = finish_process(
        Err(anyhow::anyhow!("server failed")),
        ProcessTasks::new(tokio_util::sync::CancellationToken::new()),
        || async { Err(anyhow::anyhow!("availability failed")) },
    )
    .await
    .unwrap_err()
    .to_string();

    assert!(error.contains("public server: server failed"), "{error}");
    assert!(error.contains("availability shutdown: availability failed"), "{error}");
}

#[tokio::test]
async fn test_plain_availability_listener_reports_address_and_stops() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let listener = prepared_plain_availability_listener(listener).unwrap();

    assert_eq!(listener.address(), address);
    listener.serve(axum::Router::new(), cancelled()).unwrap().await.unwrap();
}

#[tokio::test]
async fn test_tls_availability_listener_reports_address_and_stops() {
    let directory = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let listener = prepared_availability_listener(
        listener,
        AvailabilityListenerTransport::Tls(tls_config(&directory).await),
    )
    .unwrap();

    assert_eq!(listener.address(), address);
    listener.serve(axum::Router::new(), cancelled()).unwrap().await.unwrap();
}

#[tokio::test]
async fn test_prepared_http_serves_before_shutdown() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(
        PreparedPublicServer(prepared_http(listener, make_service(), 1, shutdown.clone()).unwrap()).serve(),
    );
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.ends_with("ready"), "{response}");
    shutdown.cancel();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_prepared_tls_stops_on_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();

    PreparedPublicServer(prepared_tls(listener, tls_config(&directory).await, make_service(), 0, cancelled()).unwrap())
        .serve()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_manual_tls_entrypoints_stop_on_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins();
    let mut config = local_config(&directory, &plugins);
    let (cert, key) = certificate_files(&directory);
    config.tls = Some(TlsConfig::Manual {
        cert: cert.clone(),
        key: key.clone(),
    });
    config.availability_listener = Some(AvailabilityListenerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        tls: Some(AvailabilityListenerTls { cert, key }),
        allow_remote_plaintext: false,
    });

    prepare_public_server(
        &config,
        config.listen_address().unwrap(),
        axum::Router::new(),
        cancelled(),
    )
    .await
    .unwrap()
    .serve()
    .await
    .unwrap();
    prepare_availability_listener(&config)
        .await
        .unwrap()
        .unwrap()
        .serve(axum::Router::new(), cancelled())
        .unwrap()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_acme_entrypoint_stops_on_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins();
    let mut config = local_config(&directory, &plugins);
    config.tls = Some(TlsConfig::Acme(AcmeConfig {
        domains: vec!["localhost".to_owned()],
        contact: "operator@example.test".to_owned(),
        cache_dir: directory.path().join("acme"),
        staging: true,
    }));

    prepare_public_server(
        &config,
        config.listen_address().unwrap(),
        axum::Router::new(),
        cancelled(),
    )
    .await
    .unwrap()
    .serve()
    .await
    .unwrap();
}

#[test]
fn test_log_nodelay_accepts_success_and_failure() {
    log_nodelay(Ok(()));
    log_nodelay(Err(std::io::Error::other("nodelay failed")));
}

#[test]
fn test_rustls_provider_install_is_idempotent() {
    install_rustls_provider();
    install_rustls_provider();
}

struct FailingWriter;

impl std::io::Write for FailingWriter {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("write failed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn test_banner_rendering_covers_terminal_modes_and_write_failure() {
    let address = "127.0.0.1:8080".parse().unwrap();
    let mut hidden = Vec::new();
    write_banner(
        &mut hidden,
        false,
        BannerStyle {
            unicode: false,
            colour: "",
        },
        &address,
        0,
        "http",
    )
    .unwrap();
    assert!(hidden.is_empty());

    for (unicode, colour, indexes, expected) in [
        (
            true,
            "\x1b[38;5;208m",
            1,
            "1 index, listening on https://127.0.0.1:8080",
        ),
        (false, "", 2, "2 indexes, listening on https://127.0.0.1:8080"),
    ] {
        let mut output = Vec::new();
        write_banner(
            &mut output,
            true,
            BannerStyle { unicode, colour },
            &address,
            indexes,
            "https",
        )
        .unwrap();
        assert!(String::from_utf8(output).unwrap().contains(expected));
    }

    write_banner_logged(
        &mut FailingWriter,
        true,
        BannerStyle {
            unicode: false,
            colour: "",
        },
        &address,
        1,
        "http",
    );
    std::io::Write::flush(&mut FailingWriter).unwrap();
    print_banner(&address, 1, "http");
}

#[test]
fn test_banner_style_covers_terminal_capabilities() {
    assert!(banner_environment().locale.is_ascii());
    for (environment, expected) in [
        (
            BannerEnvironment {
                locale: "en_us.utf-8".to_owned(),
                no_color: true,
                color_term: "truecolor".to_owned(),
                term: "xterm-256color".to_owned(),
            },
            (true, ""),
        ),
        (
            BannerEnvironment {
                locale: "C".to_owned(),
                no_color: false,
                color_term: "24bit".to_owned(),
                term: String::new(),
            },
            (false, "\x1b[38;2;247;120;0m"),
        ),
        (
            BannerEnvironment {
                locale: "c.utf8".to_owned(),
                no_color: false,
                color_term: String::new(),
                term: "xterm-256color".to_owned(),
            },
            (true, "\x1b[38;5;208m"),
        ),
        (
            BannerEnvironment {
                locale: "C".to_owned(),
                no_color: false,
                color_term: String::new(),
                term: String::new(),
            },
            (false, ""),
        ),
    ] {
        let style = banner_style(&environment);
        assert_eq!((style.unicode, style.colour), expected);
    }
}

#[tokio::test]
async fn test_acme_helpers_cover_shutdown_events_and_join_failures() {
    let cancellation = cancelled();
    drive_acme(futures_util::stream::pending::<Result<(), &str>>(), cancellation)
        .await
        .unwrap();
    assert_eq!(
        drive_acme(
            futures_util::stream::iter([Ok::<_, &str>("renewed"), Err("failed")]),
            tokio_util::sync::CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string(),
        "ACME state failed: failed"
    );
    assert_eq!(
        drive_acme(
            futures_util::stream::empty::<Result<(), &str>>(),
            tokio_util::sync::CancellationToken::new()
        )
        .await
        .unwrap_err()
        .to_string(),
        "ACME state reached unexpected EOF"
    );

    assert!(join_acme_task(tokio::spawn(async { Ok(()) }).await).is_ok());
    assert_eq!(
        join_acme_task(tokio::spawn(async { Err(anyhow::anyhow!("task failed")) }).await)
            .unwrap_err()
            .to_string(),
        "task failed"
    );
    assert!(
        join_acme_task(tokio::spawn(async { panic!("task panicked") }).await)
            .unwrap_err()
            .to_string()
            .contains("join ACME task")
    );
}

#[tokio::test]
async fn test_acme_supervision_reports_early_worker_failure() {
    let server_stop = tokio_util::sync::CancellationToken::new();
    let server = server_stop.clone();
    let acme_task = tokio::spawn(async { anyhow::bail!("ACME worker failed") });
    let error = supervise_acme(
        Box::pin(async move {
            server.cancelled_owned().await;
            Ok(())
        }),
        acme_task,
        tokio_util::sync::CancellationToken::new(),
        tokio_util::sync::CancellationToken::new(),
        move || server_stop.cancel(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.to_string(), "ACME worker failed");
}

#[tokio::test]
async fn test_acme_supervision_stops_worker_after_server_completion() {
    let acme_shutdown = tokio_util::sync::CancellationToken::new();
    let worker_shutdown = acme_shutdown.clone();
    let acme_task = tokio::spawn(async move {
        worker_shutdown.cancelled_owned().await;
        Ok(())
    });

    supervise_acme(
        Box::pin(async { Ok(()) }),
        acme_task,
        tokio_util::sync::CancellationToken::new(),
        acme_shutdown,
        std::thread::yield_now,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn test_prepared_acme_stops_on_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let acme = AcmeConfig {
        domains: vec!["localhost".to_owned()],
        contact: "operator@example.test".to_owned(),
        cache_dir: directory.path().join("acme"),
        staging: true,
    };

    PreparedPublicServer(prepared_acme(listener, acme, make_service(), 0, cancelled()).unwrap())
        .serve()
        .await
        .unwrap();
}

#[test]
fn test_config_snippet_command_uses_active_plugins() {
    use clap::Parser as _;

    let plugins = plugins();
    let cli = Cli::parse_from([
        "peryx",
        "config-snippet",
        "--base-url",
        "https://packages.example/cache",
        "--index",
        "main",
        "client.conf",
    ]);

    run_with_plugins(cli, &plugins).unwrap();
}

#[cfg(feature = "self-update")]
#[test]
fn test_update_message_reports_current_and_new_versions() {
    assert_eq!(update_message(None), "peryx is already up to date");
    assert_eq!(update_message(Some("1.2.3")), "updated to 1.2.3");
}

#[test]
fn test_local_server_stops_with_writable_cached_index() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins();
    let mut config = local_config(&directory, &plugins);
    config.indexes[0].kind = IndexKind::Cached {
        routing: UpstreamRoutingConfig {
            upstreams: vec![UpstreamConfig {
                name: "primary".to_owned(),
                url: "http://127.0.0.1:1".to_owned(),
                artifact_url: None,
                username: None,
                password: None,
                token: None,
                credential_exec: None,
                credential_refresh: None,
                tls: UpstreamTlsConfig::default(),
            }],
            fallback: true,
            protected: Vec::new(),
            pins: BTreeMap::new(),
        },
        upstream_concurrency: 1,
        offline: false,
        prefetch: Box::new(PrefetchConfig::default()),
    };
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();

    run_server_until_with_active_plugins(&config, &active, cancelled()).unwrap();
}

#[test]
fn test_local_server_completes_cache_warming_before_shutdown() {
    use std::io::{BufRead as _, Read as _, Write as _};

    let upstream = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    let responder = std::thread::spawn(move || {
        let result = (|| -> std::io::Result<String> {
            let (stream, _) = upstream.accept()?;
            let mut stream = std::io::BufReader::new(stream);
            let mut request_line = String::new();
            stream.read_line(&mut request_line)?;
            let mut header = String::new();
            while stream.read_line(&mut header)? != 0 && header != "\r\n" {
                header.clear();
            }
            stream
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            stream.get_mut().shutdown(std::net::Shutdown::Write)?;
            let mut discarded = Vec::new();
            stream.read_to_end(&mut discarded)?;
            Ok(request_line)
        })();
        stop.cancel();
        result
    });
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins();
    let mut config = local_config(&directory, &plugins);
    config.indexes[0].kind = IndexKind::Cached {
        routing: UpstreamRoutingConfig {
            upstreams: vec![UpstreamConfig {
                name: "primary".to_owned(),
                url: format!("http://{upstream_address}"),
                artifact_url: None,
                username: None,
                password: None,
                token: None,
                credential_exec: None,
                credential_refresh: None,
                tls: UpstreamTlsConfig::default(),
            }],
            fallback: true,
            protected: Vec::new(),
            pins: BTreeMap::new(),
        },
        upstream_concurrency: 1,
        offline: false,
        prefetch: Box::new(PrefetchConfig::default()),
    };
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();

    run_server_until_with_active_plugins(&config, &active, shutdown).unwrap();
    assert_eq!(responder.join().unwrap().unwrap(), "HEAD / HTTP/1.1\r\n");
}

#[test]
fn test_read_only_local_server_stops_without_background_tasks() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins();
    let mut config = local_config(&directory, &plugins);
    config.read_only = true;
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();

    run_server_until_with_active_plugins(&config, &active, cancelled()).unwrap();
}

#[test]
fn test_local_server_stops_its_configured_webhook_worker() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins();
    let mut config = local_config(&directory, &plugins);
    config.indexes[0].webhooks.push(WebhookConfig {
        name: "audit".to_owned(),
        url: "http://127.0.0.1:1/events".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: vec!["upload".to_owned()],
    });
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();
    run_server_until_with_active_plugins(&config, &active, cancelled()).unwrap();
}

#[cfg(unix)]
#[test]
fn test_inherited_listener_descriptor_validates_the_bound_address() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let inherited =
        inherited_listener_from_descriptor(listener.try_clone().unwrap().into(), address, "TEST_FD").unwrap();
    assert_eq!(inherited.local_addr().unwrap(), address);

    let error =
        inherited_listener_from_descriptor(listener.into(), "127.0.0.1:1".parse().unwrap(), "TEST_FD").unwrap_err();
    assert!(error.to_string().contains("expected 127.0.0.1:1"), "{error}");
    drop(duplicate_inherited_descriptor(2, "TEST_FD").unwrap());
    assert!(
        duplicate_inherited_descriptor(i32::MAX, "TEST_FD")
            .unwrap_err()
            .to_string()
            .contains("duplicate listener descriptor from TEST_FD")
    );
}

#[test]
fn test_distributed_server_installs_plugin_runtime_and_stops() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins_without_retention();
    let mut config = local_config(&directory, &plugins);
    config.writer_identity = Some("writer".to_owned());
    config.availability = AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "writer".to_owned(),
        token: SecretSource::Literal("replication-token".to_owned()),
    });
    config.dc_membership = Some(DcMembership {
        group: "group-a".to_owned(),
        members: vec![DcMember {
            node: "writer".to_owned(),
            dc: "dc-a".to_owned(),
            address: "http://127.0.0.1:9000".to_owned(),
            role: DcRole::Writer,
        }],
    });
    config.availability_listener = Some(AvailabilityListenerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        tls: None,
        allow_remote_plaintext: false,
    });
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();
    run_server_until_with_active_plugins(&config, &active, cancelled()).unwrap();

    config.indexes[0].webhooks.push(WebhookConfig {
        name: "audit".to_owned(),
        url: "http://127.0.0.1:1/events".to_owned(),
        secret: WebhookSecret::Literal("secret".to_owned()),
        events: vec!["upload".to_owned()],
    });
    run_server_until_with_active_plugins(&config, &active, cancelled()).unwrap();

    config.read_only = true;
    run_server_until_with_active_plugins(&config, &active, cancelled()).unwrap();
}

#[test]
fn test_distributed_server_releases_listener_after_public_bind_failure() {
    let public_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let public_address = public_listener.local_addr().unwrap();
    let availability_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let availability_address = availability_listener.local_addr().unwrap();
    drop(availability_listener);
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins_without_retention();
    let mut config = local_config(&directory, &plugins);
    config.host = public_address.ip().to_string();
    config.port = public_address.port();
    config.writer_identity = Some("writer".to_owned());
    config.availability = AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "writer".to_owned(),
        token: SecretSource::Literal("replication-token".to_owned()),
    });
    config.availability_listener = Some(AvailabilityListenerConfig {
        bind: availability_address,
        tls: None,
        allow_remote_plaintext: false,
    });
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();

    let error = run_server_until_with_active_plugins(&config, &active, cancelled()).unwrap_err();

    assert!(
        error
            .to_string()
            .contains(&format!("bind HTTP listener on {public_address}"))
    );
    drop(std::net::TcpListener::bind(availability_address).unwrap());
}

struct FailingAvailabilityListener(std::net::SocketAddr);

impl peryx_ha_distributed::PreparedAvailabilityListener for FailingAvailabilityListener {
    fn address(&self) -> std::net::SocketAddr {
        self.0
    }

    fn serve(
        self: Box<Self>,
        _: axum::Router,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<peryx_ha_distributed::AvailabilityListenerFuture, peryx_ha_distributed::AvailabilityListenerError> {
        Err(peryx_ha_distributed::AvailabilityListenerError::Setup(format!(
            "injected failure at {}",
            self.address()
        )))
    }
}

#[tokio::test]
async fn test_prepared_process_rolls_back_an_activation_failure() {
    let directory = tempfile::tempdir().unwrap();
    let plugins = plugins_without_retention();
    let mut config = local_config(&directory, &plugins);
    config.writer_identity = Some("writer".to_owned());
    config.availability = AvailabilityConfig::Dc(ReplicationConfig::Primary {
        source: "writer".to_owned(),
        token: SecretSource::Literal("replication-token".to_owned()),
    });
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();
    let state = crate::server::build_state_with_active_plugins(&config, &active).unwrap();
    let router = crate::server::router_for(state.clone());
    let availability = prepare_distributed_availability(
        &config,
        &active,
        &state,
        Some(Box::new(FailingAvailabilityListener("127.0.0.1:1".parse().unwrap()))),
    )
    .await
    .unwrap();

    let error = run_prepared_process(
        &config,
        config.listen_address().unwrap(),
        state,
        router,
        Some(availability),
        cancelled(),
    )
    .await
    .unwrap_err();

    assert!(
        error.to_string().contains("injected failure at 127.0.0.1:1"),
        "{error:#}"
    );
}

#[test]
fn test_availability_listener_reuses_an_inherited_socket() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    let selected = availability_listener_or_bind(Some(listener), "127.0.0.1:1".parse().unwrap()).unwrap();

    assert_eq!(selected.local_addr().unwrap(), address);
}

#[test]
fn test_availability_listener_reports_bind_failure() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();

    let error = availability_listener_or_bind(None, address).unwrap_err();

    assert_eq!(error.to_string(), format!("bind availability listener at {address}"));
}
