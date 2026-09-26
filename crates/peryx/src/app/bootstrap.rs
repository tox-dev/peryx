//! First-administrator bootstrap, explicit or on a single node's first start.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use peryx_driver::AppState;
use peryx_driver::users::UserService;
use peryx_events::security::Event;

use crate::cli::BootstrapAdministratorArgs;
use crate::config::{AvailabilityConfig, Config};

const MAX_PASSWORD_CHARACTERS: usize = 1_024;
const MIN_PASSWORD_CHARACTERS: usize = 15;
pub const INITIAL_ADMINISTRATOR: &str = "admin";
const INITIAL_PASSWORD_FILE: &str = "initial-admin-password";
/// 192 bits, which encode to 32 URL-safe characters inside the password length policy.
const INITIAL_PASSWORD_BYTES: usize = 24;

/// Create the first administrator from a bounded secret input and print its stable identity.
///
/// # Errors
/// Returns an error for invalid secret input, a read-only runtime, password derivation failure, an
/// existing administrator, a name conflict, a metadata failure, or an output failure.
pub fn bootstrap_administrator(
    config: &Config,
    args: &BootstrapAdministratorArgs,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    bootstrap_administrator_with_plugins(config, &crate::compiled_plugins(), args, stdin, out)
}

/// # Errors
/// Returns an error for invalid input, metadata access, identity creation, or output.
pub fn bootstrap_administrator_with_plugins(
    config: &Config,
    plugins: &peryx_plugin_registry::PluginRegistry,
    args: &BootstrapAdministratorArgs,
    stdin: &mut dyn Read,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    if config.read_only {
        bail!("cannot bootstrap an administrator in read-only mode");
    }
    super::init_data_dir(&config.data_dir)
        .context(format!("initialize data directory {}", config.data_dir.display()))?;
    let password = super::secret::read_secret(args.password_file.as_deref(), stdin, "password")?;
    let characters = password.chars().count();
    if characters < MIN_PASSWORD_CHARACTERS {
        bail!("administrator password must contain at least {MIN_PASSWORD_CHARACTERS} characters");
    }
    if characters > MAX_PASSWORD_CHARACTERS {
        bail!("administrator password must contain at most {MAX_PASSWORD_CHARACTERS} characters");
    }
    let path = config.data_dir.join("peryx.redb");
    let store = crate::metadata::open(&path, plugins)?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let user = runtime.block_on(UserService::new(store).bootstrap_administrator(&args.display_name, &password))?;
    Event::new("administrator_bootstrap", "success").emit();
    writeln!(out, "administrator\t{}\t{}", user.id, user.name.display())?;
    Ok(())
}

/// Create the [`INITIAL_ADMINISTRATOR`] with a random password when a writable single-node server
/// starts on a store holding no administrator grant, and return the file the password was written to.
///
/// The password reaches the file before the account commits, so a committed account never lacks its
/// only copy of the secret; a failed bootstrap removes the file again.
///
/// # Errors
/// Returns an error when the metadata store cannot be read, the CSPRNG fails, the password file
/// already exists or cannot be written, or the bootstrap transaction refuses.
pub async fn provision_initial_administrator(config: &Config, state: &AppState) -> anyhow::Result<Option<PathBuf>> {
    if state.serving.read_only
        || !matches!(config.availability, AvailabilityConfig::None)
        || state
            .serving
            .meta
            .administrator_exists()
            .context("check for an administrator")?
    {
        return Ok(None);
    }
    let mut bytes = [0u8; INITIAL_PASSWORD_BYTES];
    getrandom::fill(&mut bytes).context("generate the initial administrator password")?;
    let password = URL_SAFE_NO_PAD.encode(bytes);
    let path = config.data_dir.join(INITIAL_PASSWORD_FILE);
    write_private_file(&path, &password).context(format!("write initial administrator password {}", path.display()))?;
    if let Err(error) = UserService::new(state.serving.meta.clone())
        .bootstrap_administrator(INITIAL_ADMINISTRATOR, &password)
        .await
    {
        std::fs::remove_file(&path).context(format!("remove initial administrator password {}", path.display()))?;
        return Err(error).context(format!("create initial administrator {INITIAL_ADMINISTRATOR:?}"));
    }
    Event::new("administrator_bootstrap", "success").emit();
    tracing::info!(password_file = %path.display(), "created initial administrator {INITIAL_ADMINISTRATOR}");
    Ok(Some(path))
}

/// Create the file exclusively so only its owner may read or write it: `0600` on Unix regardless of
/// the umask, a plain exclusive create elsewhere.
fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
#[path = "../../tests/unit/tests/app/bootstrap_tests.rs"]
mod tests;
