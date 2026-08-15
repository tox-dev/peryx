use std::io::{Read, Write};
use std::net::IpAddr;
use std::str::FromStr as _;
use std::time::Duration;

use anyhow::{Context as _, bail};
use peryx_identity::ArtifactDigest;
use reqwest::{Method, Url};

use crate::cli::RevocationCommand;

const MAX_RESPONSE_BYTES: usize = 1_048_576;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// # Errors
/// Returns an error for unsafe URLs, invalid digests, secret input, transport failures, non-success
/// responses, oversized or invalid JSON responses, or output failures.
pub fn revocation(command: &RevocationCommand, stdin: &mut dyn Read, out: &mut dyn Write) -> anyhow::Result<()> {
    let client_args = command.client();
    let base = server_url(&client_args.server)?;
    let password = super::secret::read_secret(client_args.password_file.as_deref(), stdin, "password")?;
    let request = request(command, &base)?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let document = runtime.block_on(run(&client_args.user, password, request))?;
    serde_json::to_writer(&mut *out, &document)?;
    writeln!(out)?;
    Ok(())
}

async fn run(
    user: &str,
    password: String,
    (method, url, body): (Method, Url, Option<serde_json::Value>),
) -> anyhow::Result<serde_json::Value> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()?;
    let mut request = client.request(method, url).basic_auth(user, Some(password));
    if let Some(body) = body {
        request = request
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body)?);
    }
    let mut response = request.send().await.context("send revocation request")?;
    let status = response.status();
    if !status.is_success() {
        bail!("revocation request failed with HTTP {status}");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read revocation response")? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            bail!("revocation response exceeds the {MAX_RESPONSE_BYTES}-byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("revocation response is not valid JSON")
}

fn request(command: &RevocationCommand, base: &Url) -> anyhow::Result<(Method, Url, Option<serde_json::Value>)> {
    match command {
        RevocationCommand::Put(args) => Ok((
            Method::PUT,
            digest_url(base, &args.digest, "")?,
            Some(serde_json::json!({"reason": args.reason})),
        )),
        RevocationCommand::Inspect(args) => Ok((Method::GET, digest_url(base, &args.digest, "")?, None)),
        RevocationCommand::List(args) => {
            let mut url = base.join("+revocations").context("construct revocation URL")?;
            {
                let mut query = url.query_pairs_mut();
                if let Some(status) = args.status {
                    query.append_pair("status", status.as_str());
                }
                if let Some(cursor) = &args.cursor {
                    ArtifactDigest::from_str(cursor).context("invalid revocation cursor")?;
                    query.append_pair("cursor", cursor);
                }
                if let Some(limit) = args.limit {
                    query.append_pair("limit", &limit.to_string());
                }
            }
            Ok((Method::GET, url, None))
        }
        RevocationCommand::Lift(args) => Ok((Method::POST, digest_url(base, &args.digest, "/lift")?, None)),
    }
}

fn digest_url(base: &Url, digest: &str, suffix: &str) -> anyhow::Result<Url> {
    ArtifactDigest::from_str(digest).context("invalid artifact digest")?;
    base.join(&format!("+revocations/{digest}{suffix}"))
        .context("construct revocation URL")
}

fn server_url(value: &str) -> anyhow::Result<Url> {
    let mut url = Url::parse(value).context("invalid peryx server URL")?;
    if url.username() != "" || url.password().is_some() {
        bail!("peryx server URL must not contain credentials");
    }
    let host = url.host_str().context("peryx server URL must contain a host")?;
    let loopback =
        host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(|address| address.is_loopback());
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => bail!("HTTP is allowed only for a loopback peryx server"),
        _ => bail!("peryx server URL must use HTTPS or loopback HTTP"),
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/revocation_tests.rs"]
mod tests;
