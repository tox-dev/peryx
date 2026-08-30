use std::ffi::OsString;
use std::fs;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;

use crate::{ADMIN_PASSWORD, AVAILABILITY_LISTENER_FD_ENV, PUBLIC_LISTENER_FD_ENV};

pub fn run() -> ExitCode {
    execute(
        &std::env::current_exe().expect("resolve fixture executable"),
        std::env::args_os().skip(1),
    )
}

fn execute(executable: &Path, arguments: impl Iterator<Item = OsString>) -> ExitCode {
    let arguments = arguments
        .map(|argument| argument.into_string().expect("UTF-8 fixture argument"))
        .collect::<Vec<_>>();
    let result = if executable.file_stem().is_some_and(|name| name == "toxiproxy-server") {
        run_toxiproxy(executable, &arguments, None)
    } else {
        run_peryx(executable, &arguments, None)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run_peryx(executable: &Path, args: &[String], public_listener: Option<TcpListener>) -> Result<(), String> {
    if args.starts_with(&["config".to_owned(), "check".to_owned()]) {
        let config = fs::read_to_string(argument(args, "--config")).expect("read checked config");
        if config.contains("reject = true") {
            eprintln!("rejected config");
            return Err("rejected config".to_owned());
        }
        print!("{config}");
        return Ok(());
    }
    if args.first().is_some_and(|arg| arg == "bootstrap-administrator") {
        let data = PathBuf::from(argument(args, "--data-dir"));
        assert_eq!(
            fs::read_to_string(data.join("admin-password")).expect("read admin password"),
            ADMIN_PASSWORD,
        );
        return fail_when(&sibling(executable, "bootstrap-mode"), "bootstrap rejected");
    }
    if args.starts_with(&["writer".to_owned(), "claim".to_owned()]) {
        return fail_when(&sibling(executable, "claim-mode"), "claim rejected");
    }
    if args.first().is_none_or(|arg| arg != "serve") {
        return Err("expected serve command".to_owned());
    }
    let port = argument(args, "--port").parse::<u16>().expect("public port");
    let serve_mode = fs::read_to_string(sibling(executable, "serve-mode")).expect("read serve mode");
    if serve_mode == "hang" {
        println!(r#"{{"message":"fixture process started"}}"#);
        std::io::stdout().flush().expect("flush process start event");
    }
    if matches!(serve_mode.as_str(), "hang" | "silent-hang") {
        let listener = public_listener
            .unwrap_or_else(|| fixture_listener_from_descriptor(std::env::var_os(PUBLIC_LISTENER_FD_ENV), port));
        let (mut stream, _) = listener.accept().expect("accept shutdown request");
        assert_eq!(request_path(&read_request(&mut stream)), "/__fixture/shutdown");
        write_response(&mut stream, 204, b"", 0);
        return Ok(());
    }
    let config = fs::read_to_string(argument(args, "--config")).expect("read server config");
    if config.contains("invalid = [") {
        eprintln!("invalid config");
        return Err("invalid config".to_owned());
    }
    let public = public_listener
        .unwrap_or_else(|| fixture_listener_from_descriptor(std::env::var_os(PUBLIC_LISTENER_FD_ENV), port));
    let control_server = config
        .lines()
        .find_map(|line| {
            line.strip_prefix("bind = \"127.0.0.1:")
                .and_then(|value| value.strip_suffix('"'))
        })
        .map(|control| {
            let control = fixture_listener_from_descriptor(
                std::env::var_os(AVAILABILITY_LISTENER_FD_ENV),
                control.parse::<u16>().expect("control port"),
            );
            let address = control.local_addr().expect("control listener address");
            let state = sibling(executable, "state");
            (address, thread::spawn(move || serve_peryx(&control, &state, true)))
        });
    if serve_mode != "direct-startup" {
        println!(r#"{{"message":"fixture booting"}}"#);
    }
    println!(r#"{{"message":"peryx listening"}}"#);
    println!(r#"{{"message":"fixture event"}}"#);
    std::io::stdout().flush().expect("flush startup signal");
    if serve_mode == "signal-only" {
        fs::write(sibling(executable, "state"), "status-broken").expect("reject readiness request");
        println!(r#"{{"message":"fixture signal-only"}}"#);
        std::io::stdout().flush().expect("flush signal-only event");
    }
    serve_peryx(&public, &sibling(executable, "state"), false);
    if let Some((address, server)) = control_server {
        shutdown_server(address);
        server.join().expect("join control server");
    }
    Ok(())
}

fn shutdown_server(address: std::net::SocketAddr) {
    let mut stream = TcpStream::connect(address).expect("connect to control server");
    stream
        .write_all(b"GET /__fixture/shutdown HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n")
        .expect("request control shutdown");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .expect("read control shutdown response");
    assert!(response.starts_with(b"HTTP/1.1 204"));
}

fn fail_when(mode: &Path, message: &str) -> Result<(), String> {
    if fs::read_to_string(mode).expect("read fixture mode") == "fail" {
        Err(message.to_owned())
    } else {
        Ok(())
    }
}

fn fixture_listener_from_descriptor(descriptor: Option<OsString>, port: u16) -> TcpListener {
    #[cfg(unix)]
    {
        let Some(descriptor) = descriptor else {
            return TcpListener::bind(("127.0.0.1", port)).expect("bind fixture listener");
        };
        let descriptor = descriptor
            .to_string_lossy()
            .parse()
            .expect("inherited listener descriptor");
        inherited_listener(owned_inherited_descriptor(descriptor), port)
    }
    #[cfg(not(unix))]
    {
        let _ = descriptor;
        TcpListener::bind(("127.0.0.1", port)).expect("bind fixture listener")
    }
}

#[cfg(unix)]
#[allow(unsafe_code, reason = "the launcher transfers ownership of its dedicated descriptor")]
fn owned_inherited_descriptor(descriptor: std::os::fd::RawFd) -> std::os::fd::OwnedFd {
    use std::os::fd::FromRawFd as _;

    // SAFETY: the launcher creates this descriptor for the fixture and does not retain it after exec.
    unsafe { std::os::fd::OwnedFd::from_raw_fd(descriptor) }
}

#[cfg(unix)]
fn inherited_listener(descriptor: std::os::fd::OwnedFd, port: u16) -> TcpListener {
    let listener = TcpListener::from(descriptor);
    assert_eq!(listener.local_addr().expect("fixture listener address").port(), port);
    listener
}

fn serve_peryx(listener: &TcpListener, state_path: &Path, control: bool) {
    for stream in listener.incoming() {
        let mut stream = stream.expect("accept peryx request");
        let request = read_request(&mut stream);
        let path = request_path(&request);
        if path == "/__fixture/shutdown" {
            write_response(&mut stream, 204, b"", 0);
            return;
        }
        let state = fs::read_to_string(state_path).expect("read state");
        if path == "/+availability/topology/stream" {
            if state == "leader-until-stream-error" {
                fs::write(state_path, "control-500").expect("invalidate leader observation");
                write_topology_stream(&mut stream, "stream-503");
            } else {
                write_topology_stream(&mut stream, &state);
            }
            continue;
        }
        let (status, body, declared) = peryx_response(path, control, &state);
        if let Some(leader) = state.strip_prefix("transfer:") {
            fs::write(state_path, format!("leader:{leader}")).expect("complete leader transfer");
        }
        write_response(&mut stream, status, &body, declared);
    }
}

fn write_topology_stream(stream: &mut TcpStream, state: &str) {
    if state == "stream-503" {
        write_response(stream, 503, b"unavailable", 11);
        return;
    }
    if state == "stream-broken" {
        write!(
            stream,
            "HTTP/1.1 200 test\r\nTransfer-Encoding: chunked\r\nContent-Type: text/event-stream\r\n\r\ninvalid\r\n"
        )
        .expect("write malformed topology stream");
        return;
    }
    write!(
        stream,
        "HTTP/1.1 200 test\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
    )
    .expect("write topology stream head");
    if state == "stream-silent" {
        stream.flush().expect("flush topology stream head");
        let _ = stream.read(&mut [0]);
        return;
    }
    write!(stream, "event: topology\ndata: {{}}\n\n").expect("write topology event");
}

fn peryx_response(path: &str, control: bool, state: &str) -> (u16, Vec<u8>, usize) {
    let response = if matches!((control, path), (true, "/availability/v1/status")) {
        match state {
            "control-500" => (500, b"error".to_vec()),
            "control-invalid" => (200, b"{".to_vec()),
            "control-empty" => (200, b"{}".to_vec()),
            value => (
                200,
                format!(
                    "{{\"consensus\":{{\"leader\":\"{}\"}}}}",
                    value.strip_prefix("leader:").unwrap_or("dc-a"),
                )
                .into_bytes(),
            ),
        }
    } else {
        match path {
            "/+status" => (200, b"{\"version\":\"test\"}".to_vec()),
            "/+ready" => (200, b"ready".to_vec()),
            "/+availability/topology" => (200, b"topology".to_vec()),
            "/+availability/placements" => (200, b"placements".to_vec()),
            "/metrics" => (200, b"metric 1\n".to_vec()),
            "/text" => (200, b"text".to_vec()),
            "/admin" => (200, b"admin".to_vec()),
            "/binary" => (200, vec![0, 159, 146, 150]),
            "/request" => (201, Vec::new()),
            "/broken" => (200, b"x".to_vec()),
            _ => (404, b"missing".to_vec()),
        }
    };
    let declared = if path == "/broken" || (path == "/+status" && state == "status-broken") {
        100
    } else {
        response.1.len()
    };
    (response.0, response.1, declared)
}

fn run_toxiproxy(executable: &Path, args: &[String], control_listener: Option<TcpListener>) -> Result<(), String> {
    fs::write(sibling(executable, "toxi-pid"), std::process::id().to_string()).expect("write toxiproxy pid");
    let mode = fs::read_to_string(sibling(executable, "toxi-mode")).expect("read toxiproxy mode");
    let mode = mode.strip_prefix("event-").map_or(mode.as_str(), |mode| {
        println!(r#"{{"message":"fixture process started"}}"#);
        std::io::stdout().flush().expect("flush process start event");
        mode
    });
    if mode == "exit" {
        return Ok(());
    }
    if mode == "signal-exit" {
        emit_toxiproxy_startup();
        return Ok(());
    }
    if let Some(port) = mode.strip_prefix("silent-gate:") {
        let mut gate =
            TcpStream::connect(("127.0.0.1", port.parse::<u16>().expect("gate port"))).expect("connect startup gate");
        gate.write_all(&[1]).expect("identify startup gate");
    }
    #[cfg(unix)]
    if let Some(path) = mode.strip_prefix("external-reap:") {
        transfer_output_descriptors(path);
        return Ok(());
    }
    let mut readiness_gate = mode.strip_prefix("gate:").map(|port| {
        emit_toxiproxy_startup();
        let mut gate =
            TcpStream::connect(("127.0.0.1", port.parse::<u16>().expect("gate port"))).expect("connect readiness gate");
        gate.write_all(
            &argument(args, "-port")
                .parse::<u16>()
                .expect("control port")
                .to_be_bytes(),
        )
        .expect("publish control port");
        gate.read_to_end(&mut Vec::new()).expect("wait for readiness cleanup");
        gate
    });
    let listener = control_listener.unwrap_or_else(|| {
        TcpListener::bind((
            "127.0.0.1",
            argument(args, "-port").parse::<u16>().expect("control port"),
        ))
        .expect("bind toxiproxy control")
    });
    if mode == "ready" || mode.starts_with("shutdown-gate:") {
        emit_toxiproxy_startup();
    }
    if let Some(gate) = &mut readiness_gate {
        gate.write_all(&[1]).expect("acknowledge control bind");
    }
    let mut proxies = std::collections::HashSet::new();
    loop {
        let (mut stream, _) = listener.accept().expect("accept toxiproxy request");
        let request = read_request(&mut stream);
        if request.starts_with("POST /shutdown ") {
            if let Some(port) = mode.strip_prefix("shutdown-gate:") {
                let mut gate = TcpStream::connect(("127.0.0.1", port.parse::<u16>().expect("gate port")))
                    .expect("connect shutdown gate");
                gate.write_all(&[1]).expect("publish shutdown request");
                write_response(&mut stream, 404, b"{}", 2);
                gate.read_exact(&mut [0]).expect("wait for shutdown cleanup");
            } else {
                write_response(&mut stream, 204, b"", 0);
            }
            return Ok(());
        }
        let state = fs::read_to_string(sibling(executable, "toxi-state")).expect("read toxiproxy state");
        let status = if state == "startup-not-found" && request.starts_with("GET /version ") {
            fs::write(sibling(executable, "toxi-state"), "ok").expect("release toxiproxy readiness");
            404
        } else if state == "startup-error" || state == "error" && !request.starts_with("GET /version ") {
            500
        } else if request.starts_with("POST /proxies ") && !proxies.insert(proxy_name(&request)) {
            409
        } else {
            200
        };
        let body: &[u8] = if !request.starts_with("POST /proxies ") {
            b"{}"
        } else if state == "proxy-malformed" {
            b"{"
        } else if state == "proxy-missing-listen" {
            b"{}"
        } else {
            br#"{"listen":"127.0.0.1:23456"}"#
        };
        write_response(&mut stream, status, body, body.len());
    }
}

#[cfg(unix)]
fn transfer_output_descriptors(path: &str) {
    use std::os::fd::AsFd as _;
    use unix_ancillary::UnixStreamExt as _;

    let socket = UnixStream::connect(path).expect("connect output descriptor socket");
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    assert_eq!(
        socket
            .send_fds(&[1], &[stdout.as_fd(), stderr.as_fd()])
            .expect("transfer output descriptors"),
        1,
    );
}

fn proxy_name(request: &str) -> String {
    let body = request.split_once("\r\n\r\n").expect("proxy request has a body").1;
    serde_json::from_str::<serde_json::Value>(body).expect("parse proxy request")["name"]
        .as_str()
        .expect("proxy request has a name")
        .to_owned()
}

fn emit_toxiproxy_startup() {
    println!(r#"{{"message":"Starting Toxiproxy HTTP server"}}"#);
    std::io::stdout().flush().expect("flush startup signal");
}

fn sibling(executable: &Path, name: &str) -> PathBuf {
    executable.parent().expect("fixture directory").join(name)
}

fn argument<'a>(args: &'a [String], name: &str) -> &'a str {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map_or_else(|| panic!("missing argument {name}"), |pair| pair[1].as_str())
}

fn request_path(request: &str) -> &str {
    request.split_whitespace().nth(1).unwrap_or("/")
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    loop {
        let read = reader.read_until(b'\n', &mut bytes).expect("read request header");
        if matches!((read, bytes.ends_with(b"\r\n\r\n")), (0, _) | (_, true)) {
            break;
        }
    }
    let header_len = bytes.len();
    bytes.resize(header_len + request_body_len(&bytes), 0);
    reader.read_exact(&mut bytes[header_len..]).expect("read request body");
    String::from_utf8_lossy(&bytes).into_owned()
}

fn request_body_len(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .map_or(0, |value| value.parse::<usize>().expect("content length"))
}

fn write_response(stream: &mut TcpStream, status: u16, body: &[u8], declared: usize) {
    write!(
        stream,
        "HTTP/1.1 {status} test\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
    )
    .expect("write response head");
    stream.write_all(body).expect("write response body");
}

#[cfg(test)]
#[path = "../tests/unit/process_fixture.rs"]
mod tests;
