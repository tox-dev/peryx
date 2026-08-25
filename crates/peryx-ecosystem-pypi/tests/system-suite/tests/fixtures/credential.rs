use std::io::{Read as _, Write as _};

fn main() {
    let mut arguments = std::env::args_os().skip(1);
    let requests = arguments.next().expect("credential request path");
    let executions = arguments.next().expect("credential execution path");
    assert!(arguments.next().is_none(), "unexpected credential fixture argument");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&executions)
        .expect("open credential executions")
        .write_all(b"x")
        .expect("write credential execution");
    let mut request = Vec::new();
    std::io::stdin()
        .read_to_end(&mut request)
        .expect("read credential request");
    std::fs::write(requests, request).expect("write credential request");
    std::io::stdout()
        .write_all(br#"{"version":1,"expires_at":"2099-01-01T00:00:00Z","type":"bearer","token":"exec-token"}"#)
        .expect("write credential response");
}
