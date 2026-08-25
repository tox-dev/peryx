include!("../fixtures/process.rs");

pub fn assert_main_rejects_test_arguments() {
    assert_eq!(main(), std::process::ExitCode::FAILURE);
}

#[test]
fn fixture_config_check_reports_acceptance_and_rejection() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let executable = directory.path().join("peryx");
    fs::write(&executable, "").expect("create fixture executable");
    let config = sibling(&executable, "config");
    fs::write(&config, "reject = true").expect("write rejected config");
    assert_eq!(
        run_peryx(
            &executable,
            &[
                "config".to_owned(),
                "check".to_owned(),
                "--config".to_owned(),
                config.display().to_string()
            ],
            None,
        ),
        Err("rejected config".to_owned()),
    );
    fs::write(&config, "accepted = true").expect("write accepted config");
    assert_eq!(
        run_peryx(
            &executable,
            &[
                "config".to_owned(),
                "check".to_owned(),
                "--config".to_owned(),
                config.display().to_string(),
            ],
            None,
        ),
        Ok(()),
    );
}

#[test]
fn fixture_bootstrap_and_claim_report_configured_outcomes() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let executable = directory.path().join("peryx");
    fs::write(&executable, "").expect("create fixture executable");
    fs::write(sibling(&executable, "bootstrap-mode"), "fail").expect("set bootstrap mode");
    fs::write(sibling(&executable, "claim-mode"), "fail").expect("set claim mode");
    fs::write(directory.path().join("admin-password"), ADMIN_PASSWORD).expect("write admin password");
    let bootstrap = [
        "bootstrap-administrator".to_owned(),
        "--data-dir".to_owned(),
        directory.path().display().to_string(),
    ];

    assert_eq!(
        run_peryx(&executable, &bootstrap, None),
        Err("bootstrap rejected".to_owned()),
    );
    assert_eq!(
        run_peryx(&executable, &["writer".to_owned(), "claim".to_owned()], None),
        Err("claim rejected".to_owned()),
    );
    fs::write(sibling(&executable, "bootstrap-mode"), "ok").expect("accept bootstrap");
    fs::write(sibling(&executable, "claim-mode"), "ok").expect("accept claim");
    assert_eq!(run_peryx(&executable, &bootstrap, None), Ok(()));
    assert_eq!(
        run_peryx(&executable, &["writer".to_owned(), "claim".to_owned()], None),
        Ok(())
    );
}

#[test]
fn fixture_serve_rejects_missing_and_invalid_configuration() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let executable = directory.path().join("peryx");
    fs::write(&executable, "").expect("create fixture executable");
    assert_eq!(
        run_peryx(&executable, &[], None),
        Err("expected serve command".to_owned())
    );
    let config = sibling(&executable, "config");
    fs::write(&config, "invalid = [").expect("write invalid server config");
    fs::write(sibling(&executable, "serve-mode"), "ready").expect("set serve mode");
    assert_eq!(
        run_peryx(
            &executable,
            &[
                "serve".to_owned(),
                "--port".to_owned(),
                "0".to_owned(),
                "--config".to_owned(),
                config.display().to_string(),
            ],
            None,
        ),
        Err("invalid config".to_owned()),
    );
}

#[test]
fn fixture_execute_dispatches_from_the_executable_name() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let executable = directory.path().join("peryx");
    fs::write(&executable, "").expect("create fixture executable");
    let config = sibling(&executable, "config");
    fs::write(&config, "accepted = true").expect("write accepted config");
    fs::write(directory.path().join("toxiproxy-server"), "").expect("create toxiproxy executable");
    fs::write(sibling(&executable, "toxi-mode"), "exit").expect("set toxiproxy exit mode");
    assert_eq!(
        execute(&directory.path().join("toxiproxy-server"), std::iter::empty()),
        std::process::ExitCode::SUCCESS,
    );
    assert_eq!(
        execute(
            &executable,
            [
                OsString::from("config"),
                OsString::from("check"),
                OsString::from("--config"),
                config.into_os_string(),
            ]
            .into_iter(),
        ),
        std::process::ExitCode::SUCCESS,
    );
}

#[test]
fn fixture_protocol_helpers_cover_responses() {
    let cases = [
        ("/+status", false, "leader:dc-a"),
        ("/+ready", false, "leader:dc-a"),
        ("/+availability/topology", false, "leader:dc-a"),
        ("/+availability/placements", false, "leader:dc-a"),
        ("/metrics", false, "leader:dc-a"),
        ("/text", false, "leader:dc-a"),
        ("/admin", false, "leader:dc-a"),
        ("/binary", false, "leader:dc-a"),
        ("/request", false, "leader:dc-a"),
        ("/broken", false, "leader:dc-a"),
        ("/missing", false, "leader:dc-a"),
        ("/availability/v1/status", true, "control-500"),
        ("/availability/v1/status", true, "control-invalid"),
        ("/availability/v1/status", true, "control-empty"),
        ("/availability/v1/status", true, "leader:dc-b"),
    ];
    for (path, control, state) in cases {
        let _ = peryx_response(path, control, state);
    }
    assert_eq!(request_path("GET /path HTTP/1.1"), "/path");
    assert_eq!(request_path(""), "/");
    assert_eq!(request_body_len(b"Content-Length: 3\r\n\r\n"), 3);
    assert_eq!(request_body_len(b"\r\n"), 0);
}

#[test]
fn fixture_servers_follow_protocol_events() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let state = directory.path().join("state");
    fs::write(&state, "leader:dc-a").expect("write fixture state");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let address = listener.local_addr().expect("fixture address");
    let server_state = state.clone();
    let server = thread::spawn(move || serve_peryx(&listener, &server_state, false));
    assert!(request(address, "GET /+status HTTP/1.1\r\n\r\n").contains("200 test"));
    assert!(request(address, "GET /+availability/topology/stream HTTP/1.1\r\n\r\n").contains("event: topology"));
    fs::write(&state, "transfer:dc-b").expect("schedule fixture transfer");
    assert!(request(address, "GET /+status HTTP/1.1\r\n\r\n").contains("200 test"));
    assert_eq!(
        fs::read_to_string(&state).expect("read transferred state"),
        "leader:dc-b"
    );
    assert!(request(address, "GET /__fixture/shutdown HTTP/1.1\r\n\r\n").contains("204 test"));
    server.join().expect("join fixture server");

    for state in ["stream-503", "stream-broken"] {
        let (mut server, mut client) = socket_pair();
        write_topology_stream(&mut server, state);
        drop(server);
        let mut response = String::new();
        client.read_to_string(&mut response).expect("read topology response");
        assert!(!response.is_empty());
    }
    let (mut server, client) = socket_pair();
    client
        .shutdown(std::net::Shutdown::Write)
        .expect("close topology request");
    write_topology_stream(&mut server, "stream-silent");

    #[cfg(unix)]
    {
        use std::os::fd::{AsFd as _, IntoRawFd as _};

        drop(fixture_listener_from_descriptor(None, 0));
        let inherited = TcpListener::bind("127.0.0.1:0").expect("bind inherited listener");
        let port = inherited.local_addr().expect("inherited listener address").port();
        let descriptor = inherited
            .as_fd()
            .try_clone_to_owned()
            .expect("duplicate inherited test listener");
        drop(fixture_listener_from_descriptor(
            Some(descriptor.into_raw_fd().to_string().into()),
            port,
        ));
        assert!(std::panic::catch_unwind(|| fixture_listener_from_descriptor(Some("invalid".into()), port)).is_err());
    }
}

#[test]
fn fixture_commands_serve_through_explicit_listeners() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let executable = directory.path().join("peryx");
    fs::write(&executable, "").expect("create fixture executable");
    fs::write(sibling(&executable, "state"), "leader:dc-a").expect("write fixture state");
    fs::write(sibling(&executable, "serve-mode"), "ready").expect("set serve mode");
    let config = directory.path().join("peryx.toml");
    fs::write(&config, "").expect("write fixture config");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind command listener");
    let address = listener.local_addr().expect("command listener address");
    let command_executable = executable.clone();
    let command_config = config.clone();
    let command = thread::spawn(move || {
        run_peryx(
            &command_executable,
            &[
                "serve".to_owned(),
                "--port".to_owned(),
                address.port().to_string(),
                "--config".to_owned(),
                command_config.display().to_string(),
            ],
            Some(listener),
        )
    });
    assert!(request(address, "GET /+status HTTP/1.1\r\n\r\n").contains("200 test"));
    assert!(request(address, "GET /__fixture/shutdown HTTP/1.1\r\n\r\n").contains("204 test"));
    assert_eq!(command.join().expect("join fixture command"), Ok(()));

    fs::write(sibling(&executable, "serve-mode"), "hang").expect("set hang mode");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind hang listener");
    let address = listener.local_addr().expect("hang listener address");
    let hang_executable = executable.clone();
    let command = thread::spawn(move || {
        run_peryx(
            &hang_executable,
            &["serve".to_owned(), "--port".to_owned(), address.port().to_string()],
            Some(listener),
        )
    });
    assert!(request(address, "GET /__fixture/shutdown HTTP/1.1\r\n\r\n").contains("204 test"));
    assert_eq!(command.join().expect("join hanging command"), Ok(()));

    fs::write(sibling(&executable, "serve-mode"), "signal-only").expect("set signal-only mode");
    let public = TcpListener::bind("127.0.0.1:0").expect("bind public listener");
    let public_address = public.local_addr().expect("public listener address");
    let control = TcpListener::bind("127.0.0.1:0").expect("reserve control listener");
    let control_address = control.local_addr().expect("control listener address");
    drop(control);
    fs::write(&config, format!("bind = \"127.0.0.1:{}\"\n", control_address.port())).expect("write control config");
    let command = thread::spawn(move || {
        run_peryx(
            &executable,
            &[
                "serve".to_owned(),
                "--port".to_owned(),
                public_address.port().to_string(),
                "--config".to_owned(),
                config.display().to_string(),
            ],
            Some(public),
        )
    });
    assert!(request(public_address, "GET /+status HTTP/1.1\r\n\r\n").contains("200 test"));
    assert!(request(control_address, "GET /availability/v1/status HTTP/1.1\r\n\r\n").contains("200 test"));
    assert!(request(public_address, "GET /__fixture/shutdown HTTP/1.1\r\n\r\n").contains("204 test"));
    assert_eq!(command.join().expect("join signal-only command"), Ok(()));
}

#[test]
fn fixture_toxiproxy_uses_protocol_readiness() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let executable = directory.path().join("toxiproxy-server");
    fs::write(&executable, "").expect("create toxiproxy executable");
    fs::write(sibling(&executable, "toxi-state"), "error").expect("set toxiproxy state");
    let gate = TcpListener::bind("127.0.0.1:0").expect("bind readiness gate");
    let gate_address = gate.local_addr().expect("readiness gate address");
    fs::write(
        sibling(&executable, "toxi-mode"),
        format!("gate:{}", gate_address.port()),
    )
    .expect("set readiness gate");
    let control = TcpListener::bind("127.0.0.1:0").expect("reserve control port");
    let control_address = control.local_addr().expect("control address");
    drop(control);
    let command_executable = executable.clone();
    let command = thread::spawn(move || {
        run_toxiproxy(
            &command_executable,
            &["-port".to_owned(), control_address.port().to_string()],
            None,
        )
    });
    let mut release = super::accept_within(&gate, super::TOXIPROXY_FAILURE_TIMEOUT, "toxiproxy readiness event");
    release.read_exact(&mut [0]).expect("identify readiness gate");
    release.write_all(&[1]).expect("release readiness gate");
    release.read_exact(&mut [0]).expect("observe control bind");
    assert!(request(control_address, "POST /proxies HTTP/1.1\r\nContent-Length: 0\r\n\r\n").contains("500 test"));
    assert!(request(control_address, "GET /version HTTP/1.1\r\n\r\n").contains("200 test"));
    assert!(request(control_address, "POST /shutdown HTTP/1.1\r\nContent-Length: 0\r\n\r\n").contains("204 test"));
    assert_eq!(command.join().expect("join toxiproxy fixture"), Ok(()));

    fs::write(sibling(&executable, "toxi-mode"), "exit").expect("set exit mode");
    assert_eq!(run_toxiproxy(&executable, &[], None), Ok(()));
    fs::write(sibling(&executable, "toxi-mode"), "signal-exit").expect("set signal exit mode");
    assert_eq!(run_toxiproxy(&executable, &[], None), Ok(()));
    let gate = TcpListener::bind("127.0.0.1:0").expect("bind silent startup gate");
    fs::write(
        sibling(&executable, "toxi-mode"),
        format!("silent-gate:{}", gate.local_addr().expect("silent gate address").port()),
    )
    .expect("set silent startup mode");
    let control = TcpListener::bind("127.0.0.1:0").expect("bind silent control listener");
    let control_address = control.local_addr().expect("silent control address");
    let command_executable = executable.clone();
    let command = thread::spawn(move || run_toxiproxy(&command_executable, &[], Some(control)));
    let mut startup_signal = super::accept_within(&gate, super::TOXIPROXY_FAILURE_TIMEOUT, "silent startup event");
    startup_signal.read_exact(&mut [0]).expect("identify startup gate");
    drop(startup_signal);
    assert!(request(control_address, "POST /shutdown HTTP/1.1\r\nContent-Length: 0\r\n\r\n").contains("204 test"));
    assert_eq!(command.join().expect("join silent toxiproxy"), Ok(()));

    fs::write(sibling(&executable, "toxi-mode"), "ready").expect("set ready mode");
    fs::write(sibling(&executable, "toxi-state"), "startup-not-found").expect("delay version route");
    let control = TcpListener::bind("127.0.0.1:0").expect("bind delayed version listener");
    let control_address = control.local_addr().expect("delayed version address");
    let command_executable = executable.clone();
    let command = thread::spawn(move || run_toxiproxy(&command_executable, &[], Some(control)));
    assert!(request(control_address, "GET /version HTTP/1.1\r\n\r\n").contains("404 test"));
    assert!(request(control_address, "GET /version HTTP/1.1\r\n\r\n").contains("200 test"));
    assert!(request(control_address, "POST /shutdown HTTP/1.1\r\nContent-Length: 0\r\n\r\n").contains("204 test"));
    assert_eq!(command.join().expect("join delayed version fixture"), Ok(()));

    fs::write(sibling(&executable, "toxi-state"), "ok").expect("accept toxiproxy requests");
    let control = TcpListener::bind("127.0.0.1:0").expect("bind explicit control listener");
    let control_address = control.local_addr().expect("explicit control address");
    let command = thread::spawn(move || run_toxiproxy(&executable, &[], Some(control)));
    assert!(request(control_address, "POST /shutdown HTTP/1.1\r\nContent-Length: 0\r\n\r\n").contains("204 test"));
    assert_eq!(command.join().expect("join ready toxiproxy"), Ok(()));
}

fn request(address: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).expect("connect fixture server");
    stream.write_all(request.as_bytes()).expect("write fixture request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read fixture response");
    response
}

fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind socket pair");
    let address = listener.local_addr().expect("socket pair address");
    let client = TcpStream::connect(address).expect("connect socket pair");
    let (server, _) = listener.accept().expect("accept socket pair");
    (server, client)
}
