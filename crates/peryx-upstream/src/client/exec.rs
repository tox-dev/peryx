use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use command_group::{AsyncCommandGroup as _, AsyncGroupChild};
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
/// How far ahead of its stated expiry a helper credential is reloaded, so a request never carries a
/// credential that expires while it is in flight. A helper that returns a credential already inside this
/// margin is rejected rather than used.
const REFRESH_MARGIN: Duration = Duration::from_secs(30);

/// How long a failed helper load is left alone before another attempt, so a helper that is broken or
/// prompting is not re-run once per request. This bounds retries after a failure; a successful load
/// schedules its own reload from the credential's expiry and [`REFRESH_MARGIN`].
const FAILURE_RETRY: Duration = Duration::from_secs(30);
static EXECUTIONS: Semaphore = Semaphore::const_new(8);

/// Credential-helper authorization purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScope {
    /// Permits fetching and inspecting upstream resources.
    Read,
}

/// Bounds helper resources and omits argv from debug output.
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
    /// Accepts one absolute executable and bypasses shell interpretation of arguments.
    ///
    /// # Errors
    /// Returns a limit or shape error without exposing arguments or environment values.
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

    /// Binds a lazy provider to one upstream origin and authorization scope.
    ///
    /// # Errors
    /// Returns an origin error without exposing the configured URL.
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

    #[must_use]
    pub fn argv(&self) -> &[String] {
        &self.0.argv
    }

    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.0.timeout
    }

    #[must_use]
    pub fn environment(&self) -> &[String] {
        &self.0.environment
    }

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
        let mut command = helper_command(&self.0);
        let mut process = ProcessGroup::new(
            command
                .group()
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| CredentialError::new("credential helper failed to start"))?,
        );
        let mut stdin = process
            .child_mut()
            .inner()
            .stdin
            .take()
            .expect("credential helper stdin is piped");
        let stdout = process
            .child_mut()
            .inner()
            .stdout
            .take()
            .expect("credential helper stdout is piped");
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
                .await
                .expect("owned child stdout remains readable");
            output
        }));
        let deadline = tokio::time::Instant::now()
            .checked_add(self.0.timeout)
            .expect("validated credential timeout fits the Tokio clock");
        let mut status = None;
        let mut output = None;
        let outcome = {
            let wait = process.child_mut().inner().wait();
            tokio::pin!(wait);
            loop {
                let error = tokio::select! {
                bytes = async { output_task.as_mut().expect("output task is pending").await }, if output_task.is_some() => {
                    drop(output_task.take());
                    let bytes = bytes.expect("credential helper output task does not panic");
                    match validate_output(&bytes) {
                        Ok(()) => {
                            output = Some(bytes);
                            None
                        }
                        Err(error) => Some(error),
                    }
                }
                result = &mut wait, if status.is_none() => {
                    result.map_or_else(
                        |_| Some(CredentialError::new("credential helper wait failed")),
                        |result| {
                            status = Some(result);
                            None
                        },
                    )
                }
                () = tokio::time::sleep_until(deadline) => {
                    Some(CredentialError::new("credential helper timed out"))
                }
                };
                if let Some(error) = error {
                    break Err(error);
                }
                if status.is_some() && output.is_some() {
                    break Ok((
                        status.take().expect("status is complete"),
                        output.take().expect("output is complete"),
                    ));
                }
            }
        };
        let cleanup_result = process.terminate().await;

        match outcome {
            Ok((status, output)) => {
                let result = finish(status, input_task, output).await;
                cleanup_result?;
                result
            }
            Err(error) => {
                cleanup(input_task, &mut output_task).await;
                cleanup_result?;
                Err(error)
            }
        }
    }
}

fn helper_command(settings: &ExecCredentialSettings) -> Command {
    let mut command = Command::new(&settings.argv[0]);
    command
        .args(&settings.argv[1..])
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .envs(
            settings
                .environment
                .iter()
                .filter_map(|name| std::env::var_os(name).map(|value| (name, value))),
        );
    command
}

struct ProcessGroup {
    child: Option<AsyncGroupChild>,
}

impl ProcessGroup {
    const fn new(child: AsyncGroupChild) -> Self {
        Self { child: Some(child) }
    }

    const fn child_mut(&mut self) -> &mut AsyncGroupChild {
        self.child.as_mut().expect("process group is active")
    }

    async fn terminate(&mut self) -> Result<(), CredentialError> {
        terminate(self.child_mut())
            .await
            .map_err(|_| CredentialError::new("credential helper cleanup failed"))?;
        drop(self.child.take());
        Ok(())
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            drop(reap(child));
        }
    }
}

fn reap(mut child: AsyncGroupChild) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let _ = child.wait().await;
    })
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

async fn cleanup(
    input_task: tokio::task::JoinHandle<std::io::Result<()>>,
    output_task: &mut Option<tokio::task::JoinHandle<Vec<u8>>>,
) {
    input_task.abort();
    if let Some(output_task) = output_task.take() {
        output_task.abort();
        let _ = output_task.await;
    }
}

fn validate_output(output: &[u8]) -> Result<(), CredentialError> {
    if output.len() > MAX_OUTPUT_BYTES {
        return Err(CredentialError::new("credential helper output exceeded its limit"));
    }
    Ok(())
}

async fn terminate(child: &mut command_group::AsyncGroupChild) -> std::io::Result<()> {
    // The leader can exit before the signal; waiting for the whole group is the cleanup proof.
    let _ = child.start_kill();
    child.wait().await.map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecCredentialConfigError {
    #[error("credential helper argv must not be empty")]
    EmptyArgv,
    #[error("credential helper executable must use an absolute path")]
    RelativeExecutable,
    #[error("credential helper argv contains a null byte")]
    ArgumentNul,
    #[error("credential helper argv exceeds its limit")]
    ArgvLimit,
    #[error("credential helper environment exceeds its limit")]
    EnvironmentLimit,
    #[error("credential helper environment contains an invalid name")]
    EnvironmentName,
    #[error("credential helper timeout must be between 1 millisecond and 300 seconds")]
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExecCredentialProviderError {
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

#[cfg(test)]
#[cfg(unix)]
#[path = "../../tests/unit/client/exec/tests.rs"]
mod tests;
