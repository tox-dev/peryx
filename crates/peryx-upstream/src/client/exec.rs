use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use command_group::AsyncCommandGroup as _;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::Command;
use tokio::sync::Semaphore;
use url::Url;

use super::credential::LoadedCredential;
use super::{Auth, CredentialError, CredentialFailure, CredentialProvider, CredentialRefresh};

const MAX_ARGV_ITEMS: usize = 64;
const MAX_ARGV_BYTES: usize = 32 << 10;
const MAX_ENVIRONMENT_ITEMS: usize = 64;
const MAX_ORIGIN_BYTES: usize = 2 << 10;
const MAX_OUTPUT_BYTES: usize = 64 << 10;
const MIN_TIMEOUT: Duration = Duration::from_millis(1);
const MAX_TIMEOUT: Duration = Duration::from_mins(5);
const REFRESH_MARGIN: Duration = Duration::from_secs(30);
const FAILURE_RETRY: Duration = Duration::from_secs(30);
const PROCESS_POLL: Duration = Duration::from_millis(5);
static EXECUTIONS: Semaphore = Semaphore::const_new(8);

/// The authorization purpose sent to a credential helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScope {
    /// Fetch or inspect upstream package metadata and artifacts.
    Read,
}

/// A bounded argv credential helper. Debug output reports limits and counts without exposing argv.
#[derive(Clone, PartialEq, Eq)]
pub struct ExecCredentialConfig(Arc<ExecCredentialSettings>);

#[derive(PartialEq, Eq)]
struct ExecCredentialSettings {
    argv: Arc<[String]>,
    timeout: Duration,
    environment: Arc<[String]>,
    failure: CredentialFailure,
}

impl std::fmt::Debug for ExecCredentialConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExecCredentialConfig")
            .field("argv_items", &self.0.argv.len())
            .field("timeout", &self.0.timeout)
            .field("environment_items", &self.0.environment.len())
            .field("failure", &self.0.failure)
            .finish()
    }
}

impl ExecCredentialConfig {
    /// Validate one non-shell command and its inherited environment names.
    ///
    /// # Errors
    /// Returns a limit or shape error without including an argument or environment value.
    pub fn new(
        argv: Vec<String>,
        timeout: Duration,
        environment: Vec<String>,
        failure: CredentialFailure,
    ) -> Result<Self, ExecCredentialConfigError> {
        if argv.is_empty() {
            return Err(ExecCredentialConfigError::EmptyArgv);
        }
        if argv.iter().any(|argument| argument.contains('\0')) {
            return Err(ExecCredentialConfigError::ArgumentNul);
        }
        if !Path::new(&argv[0]).is_absolute() {
            return Err(ExecCredentialConfigError::RelativeExecutable);
        }
        if argv.len() > MAX_ARGV_ITEMS || argv.iter().map(String::len).sum::<usize>() > MAX_ARGV_BYTES {
            return Err(ExecCredentialConfigError::ArgvLimit);
        }
        if environment.len() > MAX_ENVIRONMENT_ITEMS {
            return Err(ExecCredentialConfigError::EnvironmentLimit);
        }
        if environment
            .iter()
            .any(|name| name.is_empty() || name.contains(['=', '\0']))
        {
            return Err(ExecCredentialConfigError::EnvironmentName);
        }
        if timeout < MIN_TIMEOUT || timeout > MAX_TIMEOUT {
            return Err(ExecCredentialConfigError::Timeout);
        }
        Ok(Self(Arc::new(ExecCredentialSettings {
            argv: argv.into(),
            timeout,
            environment: environment.into(),
            failure,
        })))
    }

    /// Build a lazy provider bound to one upstream origin and scope.
    ///
    /// # Errors
    /// Returns an origin error without echoing the configured URL.
    ///
    /// # Panics
    /// Panics if serde cannot encode the fixed request schema.
    pub fn provider(
        &self,
        upstream: &str,
        scope: CredentialScope,
    ) -> Result<CredentialProvider, ExecCredentialProviderError> {
        let origin = Url::parse(upstream)
            .ok()
            .filter(Url::has_host)
            .map(|url| url.origin().ascii_serialization())
            .filter(|origin| origin.len() <= MAX_ORIGIN_BYTES)
            .ok_or(ExecCredentialProviderError::Origin)?;
        let request = serde_json::to_vec(&HelperRequest {
            version: 1,
            origin: &origin,
            scope,
        })
        .expect("credential helper request is serializable");
        let config = self.clone();
        Ok(CredentialProvider::lazy(
            CredentialRefresh {
                interval: FAILURE_RETRY,
                on_unauthorized: true,
                failure: self.0.failure,
            },
            move || {
                let config = config.clone();
                let request = request.clone();
                async move { config.load(&request).await }
            },
        ))
    }

    /// The executable and fixed arguments passed directly to the operating system.
    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.0.argv
    }

    /// The longest one helper invocation may run.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.0.timeout
    }

    /// The names inherited from the parent after clearing the helper environment.
    #[must_use]
    pub fn environment(&self) -> &[String] {
        &self.0.environment
    }

    /// The behavior used while no valid cached credential is available.
    #[must_use]
    pub fn failure(&self) -> CredentialFailure {
        self.0.failure
    }

    async fn load(&self, request: &[u8]) -> Result<LoadedCredential, CredentialError> {
        let _permit = EXECUTIONS.acquire().await.expect("the static semaphore remains open");
        let output = self.execute(request).await?;
        let response: HelperResponse = serde_json::from_slice(&output)
            .map_err(|_| CredentialError::new("credential helper returned an invalid response"))?;
        if response.version() != 1 {
            return Err(CredentialError::new(
                "credential helper returned an unsupported response version",
            ));
        }
        let expires_at = OffsetDateTime::parse(response.expires_at(), &Rfc3339)
            .map_err(|_| CredentialError::new("credential helper returned an invalid expiry"))?;
        let remaining: Duration = (expires_at - OffsetDateTime::now_utc())
            .try_into()
            .map_err(|_| CredentialError::new("credential helper returned an expired credential"))?;
        let refresh_after = remaining
            .checked_sub(REFRESH_MARGIN)
            .ok_or_else(|| CredentialError::new("credential helper returned a credential inside its expiry margin"))?;
        let auth = response.auth()?;
        Ok(LoadedCredential { auth, refresh_after })
    }

    async fn execute(&self, request: &[u8]) -> Result<Vec<u8>, CredentialError> {
        let mut command = Command::new(&self.0.argv[0]);
        command
            .args(&self.0.argv[1..])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for name in self.0.environment.iter() {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command
            .group()
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| CredentialError::new("credential helper failed to start"))?;
        let mut stdin = child.inner().stdin.take().expect("credential helper stdin is piped");
        let stdout = child.inner().stdout.take().expect("credential helper stdout is piped");
        let input = request.to_vec();
        let input_task = tokio::spawn(async move {
            stdin.write_all(&input).await?;
            stdin.shutdown().await
        });
        let mut output_task = Some(tokio::spawn(async move {
            let mut output = Vec::new();
            stdout
                .take(u64::try_from(MAX_OUTPUT_BYTES + 1).expect("output limit fits u64"))
                .read_to_end(&mut output)
                .await?;
            Ok::<_, std::io::Error>(output)
        }));
        let deadline = tokio::time::Instant::now() + self.0.timeout;
        let mut output = None;
        let output = loop {
            if output_task.as_ref().is_some_and(tokio::task::JoinHandle::is_finished) {
                let bytes = output_task
                    .take()
                    .expect("unfinished output keeps its task")
                    .await
                    .expect("credential helper output task does not panic");
                let output_error = CredentialError::new("credential helper output failed");
                let bytes = bytes.or(Err(output_error))?;
                if let Err(error) = validate_output(&bytes) {
                    terminate(&mut child).await;
                    input_task.abort();
                    return Err(error);
                }
                output = Some(bytes);
            }
            let wait_error = CredentialError::new("credential helper wait failed");
            let status = child.try_wait().or(Err(wait_error))?;
            if let Some(status) = status {
                let bytes = if let Some(bytes) = output {
                    bytes
                } else {
                    let bytes = output_task
                        .take()
                        .expect("unfinished output keeps its task")
                        .await
                        .expect("credential helper output task does not panic");
                    let output_error = CredentialError::new("credential helper output failed");
                    bytes.or(Err(output_error))?
                };
                break finish(status, input_task, bytes).await;
            }
            if tokio::time::Instant::now() >= deadline {
                terminate(&mut child).await;
                input_task.abort();
                output_task.iter().for_each(tokio::task::JoinHandle::abort);
                return Err(CredentialError::new("credential helper timed out"));
            }
            tokio::time::sleep(PROCESS_POLL).await;
        }?;
        Ok(output)
    }
}

async fn finish(
    status: std::process::ExitStatus,
    input_task: tokio::task::JoinHandle<std::io::Result<()>>,
    output: Vec<u8>,
) -> Result<Vec<u8>, CredentialError> {
    let input = input_task.await.expect("credential helper input task does not panic");
    if !status.success() {
        return Err(CredentialError::new("credential helper exited unsuccessfully"));
    }
    let input_error = CredentialError::new("credential helper input failed");
    input.or(Err(input_error))?;
    validate_output(&output)?;
    Ok(output)
}

fn validate_output(output: &[u8]) -> Result<(), CredentialError> {
    if output.len() > MAX_OUTPUT_BYTES {
        return Err(CredentialError::new("credential helper output exceeded its limit"));
    }
    Ok(())
}

async fn terminate(child: &mut command_group::AsyncGroupChild) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Invalid helper command configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecCredentialConfigError {
    /// No executable was configured.
    #[error("credential helper argv must not be empty")]
    EmptyArgv,
    /// The executable path is relative.
    #[error("credential helper executable must use an absolute path")]
    RelativeExecutable,
    /// An argument contains a null byte rejected by process creation.
    #[error("credential helper argv contains a null byte")]
    ArgumentNul,
    /// The command exceeds the item or aggregate byte limit.
    #[error("credential helper argv exceeds its limit")]
    ArgvLimit,
    /// Too many environment names were configured.
    #[error("credential helper environment exceeds its limit")]
    EnvironmentLimit,
    /// An environment name is empty or contains an invalid byte.
    #[error("credential helper environment contains an invalid name")]
    EnvironmentName,
    /// The timeout is zero or exceeds five minutes.
    #[error("credential helper timeout must be between 1 millisecond and 300 seconds")]
    Timeout,
}

/// An error binding a helper to one upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecCredentialProviderError {
    /// The upstream URL has no bounded network origin.
    #[error("credential helper upstream origin is invalid or exceeds its limit")]
    Origin,
}

#[derive(Serialize)]
struct HelperRequest<'a> {
    version: u8,
    origin: &'a str,
    scope: CredentialScope,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum HelperResponse {
    Basic {
        version: u8,
        expires_at: String,
        username: String,
        password: String,
    },
    Bearer {
        version: u8,
        expires_at: String,
        token: String,
    },
}

impl HelperResponse {
    const fn version(&self) -> u8 {
        match self {
            Self::Basic { version, .. } | Self::Bearer { version, .. } => *version,
        }
    }

    fn expires_at(&self) -> &str {
        match self {
            Self::Basic { expires_at, .. } | Self::Bearer { expires_at, .. } => expires_at,
        }
    }

    fn auth(self) -> Result<Auth, CredentialError> {
        match self {
            Self::Basic { username, password, .. } if !username.is_empty() && !password.is_empty() => {
                Ok(Auth::Basic { username, password })
            }
            Self::Bearer { token, .. } if !token.is_empty() => Ok(Auth::Bearer(token)),
            Self::Basic { .. } | Self::Bearer { .. } => {
                Err(CredentialError::new("credential helper returned an empty credential"))
            }
        }
    }
}

#[cfg(all(test, unix))]
#[path = "../../tests/unit/client/exec/tests.rs"]
mod tests;
