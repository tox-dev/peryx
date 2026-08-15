//! First-administrator bootstrap without starting request-serving state.

use std::io::{Read, Write};

use anyhow::{Context as _, bail};
use peryx_driver::users::UserService;
use peryx_events::security::Event;

use crate::cli::BootstrapAdministratorArgs;
use crate::config::Config;

const MAX_PASSWORD_CHARACTERS: usize = 1_024;
const MIN_PASSWORD_CHARACTERS: usize = 15;

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

#[cfg(test)]
#[path = "../../tests/unit/tests/app/bootstrap_tests.rs"]
mod tests;
