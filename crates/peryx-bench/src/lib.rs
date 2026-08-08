//! Comparative benchmark orchestration.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use peryx_bench_core::compare;
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::{machine, report};

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ecosystem {
    Pypi,
    Oci,
}

#[derive(clap::Parser, Clone)]
pub struct Cli {
    #[arg(long, value_enum, default_value_t = Ecosystem::Pypi)]
    ecosystem: Ecosystem,

    #[arg(long, default_value_t = 3)]
    rounds: usize,

    #[arg(long, value_name = "PART")]
    skip: Vec<String>,

    #[arg(long, default_value = "")]
    only: String,

    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    #[command(subcommand)]
    mode: Option<Mode>,

    #[command(flatten)]
    pypi: peryx_ecosystem_pypi::bench::Options,

    #[command(flatten)]
    oci: peryx_ecosystem_oci::bench::Options,
}

#[derive(clap::Subcommand, Clone, Debug, PartialEq, Eq)]
enum Mode {
    VsRest,
    Ab {
        base: String,
        #[arg(long)]
        head_first: bool,
    },
}

#[derive(Debug)]
struct CommandSpec {
    program: OsString,
    args: Vec<OsString>,
    cwd: PathBuf,
    capture: bool,
}

impl CommandSpec {
    fn status(
        program: impl Into<OsString>,
        cwd: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: cwd.into(),
            capture: false,
        }
    }

    fn output(
        program: impl Into<OsString>,
        cwd: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            capture: true,
            ..Self::status(program, cwd, args)
        }
    }
}

#[derive(Debug)]
struct ProcessResult {
    success: bool,
    stdout: Vec<u8>,
}

trait Process: Send + Sync {
    fn run(&self, command: &CommandSpec) -> std::io::Result<ProcessResult>;
}

#[derive(Debug, Clone, Copy)]
struct CommandProcess;

impl Process for CommandProcess {
    fn run(&self, spec: &CommandSpec) -> std::io::Result<ProcessResult> {
        let mut command = Command::new(&spec.program);
        command.args(&spec.args).current_dir(&spec.cwd);
        if spec.capture {
            command.output().map(|output| ProcessResult {
                success: output.status.success(),
                stdout: output.stdout,
            })
        } else {
            command.status().map(|status| ProcessResult {
                success: status.success(),
                stdout: Vec::new(),
            })
        }
    }
}

trait Comparator: Send + Sync {
    fn compare(&self, base: &Path, head: &Path) -> anyhow::Result<bool>;
}

#[derive(Debug, Clone, Copy)]
struct ReportComparator;

impl Comparator for ReportComparator {
    fn compare(&self, base: &Path, head: &Path) -> anyhow::Result<bool> {
        compare::against_paths(base, head)
    }
}

#[derive(Clone)]
struct MachineProfile {
    path: PathBuf,
    scratch: PathBuf,
    settings: machine::ProfileSettings,
}

impl MachineProfile {
    fn system(root: &Path) -> Self {
        Self {
            path: root.join("site/data/bench/machine.toml"),
            scratch: std::env::temp_dir(),
            settings: machine::ProfileSettings::default(),
        }
    }

    async fn publish(&self) -> anyhow::Result<()> {
        machine::write_profile(&self.path, &self.scratch, self.settings).await
    }
}

pub struct Runner {
    core: Core<CommandProcess, ReportComparator>,
}

struct Core<P, C> {
    process: P,
    comparator: C,
    http: reqwest::Client,
    root: PathBuf,
    machine: MachineProfile,
}

impl Runner {
    /// Build the runner used by the benchmark binary.
    ///
    #[must_use]
    pub fn system() -> Self {
        let root = report::repo_root();
        Self {
            core: Core::new(
                CommandProcess,
                ReportComparator,
                root.clone(),
                MachineProfile::system(&root),
            ),
        }
    }

    /// Run the selected comparison.
    ///
    /// # Errors
    /// Returns the first build, measurement, report, or cleanup failure.
    pub async fn run(&self, cli: Cli) -> anyhow::Result<()> {
        self.core.run(cli).await
    }
}

impl<P: Process, C: Comparator> Core<P, C> {
    fn new(process: P, comparator: C, root: PathBuf, machine: MachineProfile) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self {
            process,
            comparator,
            http: reqwest::Client::new(),
            root,
            machine,
        }
    }

    async fn run(&self, cli: Cli) -> anyhow::Result<()> {
        match cli.mode.clone() {
            Some(Mode::Ab { base, head_first }) => self.ab(&base, head_first, &cli).await,
            Some(Mode::VsRest) | None => {
                let context = BenchmarkContext::new(
                    self.ensure_peryx_built()?,
                    cli.report
                        .clone()
                        .unwrap_or_else(|| self.root.join("site/data/bench/report.toml")),
                );
                self.run_suite(&context, &cli).await
            }
        }
    }

    async fn run_suite(&self, context: &BenchmarkContext, cli: &Cli) -> anyhow::Result<()> {
        if !cli.skip.iter().any(|part| part == "machine") {
            self.machine.publish().await?;
        }
        match cli.ecosystem {
            Ecosystem::Pypi => {
                peryx_ecosystem_pypi::bench::run(context, &cli.pypi, cli.rounds, &cli.skip, &cli.only, &self.http).await
            }
            Ecosystem::Oci => {
                peryx_ecosystem_oci::bench::run(context, &cli.oci, cli.rounds, &cli.skip, &cli.only, &self.http).await
            }
        }
    }

    async fn ab(&self, base_ref: &str, head_first: bool, cli: &Cli) -> anyhow::Result<()> {
        let mut suite = cli.clone();
        if suite.only.is_empty() {
            "peryx".clone_into(&mut suite.only);
        }
        if !suite.skip.iter().any(|part| part == "machine") {
            suite.skip.push("machine".to_owned());
        }
        suite.mode = None;
        let head_binary = self.ensure_peryx_built()?;
        let base_binary = self.build_base(base_ref)?;
        let base_report = self.root.join("target/bench-base-report.toml");
        let head_report = self.root.join("target/bench-head-report.toml");
        let base_context = BenchmarkContext::new(base_binary, base_report.clone());
        let head_context = BenchmarkContext::new(head_binary, head_report.clone());
        if head_first {
            self.measure("working tree", &head_context, &suite).await?;
            self.measure(&format!("base ({base_ref})"), &base_context, &suite)
                .await?;
        } else {
            self.measure(&format!("base ({base_ref})"), &base_context, &suite)
                .await?;
            self.measure("working tree", &head_context, &suite).await?;
        }
        let regressed = self.comparator.compare(&base_report, &head_report)?;
        for report in [base_report, head_report] {
            let _ = std::fs::remove_file(report);
        }
        self.remove_worktree()?;
        if regressed {
            bail!("peryx regressed against {base_ref}");
        }
        Ok(())
    }

    async fn measure(&self, label: &str, context: &BenchmarkContext, cli: &Cli) -> anyhow::Result<()> {
        println!("== measuring {label} ==");
        self.run_suite(context, cli).await
    }

    fn base_worktree(&self) -> PathBuf {
        self.root.join("target/bench-base")
    }

    fn build_base(&self, base_ref: &str) -> anyhow::Result<PathBuf> {
        let worktree = self.base_worktree();
        self.remove_worktree()?;
        println!("preparing base worktree at {}", worktree.display());
        self.run_git([
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("--detach"),
            OsStr::new("--force"),
            worktree.as_os_str(),
            OsStr::new(base_ref),
        ])?;
        println!("building peryx ({base_ref})");
        let result = self.execute(&CommandSpec::status(
            "cargo",
            &worktree,
            [
                OsString::from("build"),
                OsString::from("--release"),
                OsString::from("-p"),
                OsString::from("peryx"),
                OsString::from("--target-dir"),
                worktree.join("target").into_os_string(),
            ],
        ))?;
        if !result.success {
            bail!("base build of {base_ref} failed");
        }
        Ok(worktree
            .join("target/release")
            .join(format!("peryx{}", std::env::consts::EXE_SUFFIX)))
    }

    fn remove_worktree(&self) -> anyhow::Result<()> {
        let worktree = self.base_worktree();
        if worktree.exists() {
            self.run_git([
                OsStr::new("worktree"),
                OsStr::new("remove"),
                OsStr::new("--force"),
                worktree.as_os_str(),
            ])?;
        }
        Ok(())
    }

    fn run_git<const N: usize>(&self, args: [&OsStr; N]) -> anyhow::Result<()> {
        let printable = args
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let result = self.execute(&CommandSpec::status("git", &self.root, args))?;
        if !result.success {
            bail!("git {printable} failed");
        }
        Ok(())
    }

    fn ensure_peryx_built(&self) -> anyhow::Result<PathBuf> {
        println!("building peryx (release)");
        let build = self.execute(&CommandSpec::status(
            "cargo",
            &self.root,
            ["build", "--release", "-p", "peryx"],
        ))?;
        if !build.success {
            bail!("cargo build failed");
        }
        let output = self.execute(&CommandSpec::output(
            "cargo",
            &self.root,
            ["metadata", "--no-deps", "--format-version", "1"],
        ))?;
        if !output.success {
            bail!("cargo metadata failed");
        }
        let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        let target = metadata["target_directory"]
            .as_str()
            .context("cargo metadata omitted target_directory")?;
        Ok(PathBuf::from(target)
            .join("release")
            .join(format!("peryx{}", std::env::consts::EXE_SUFFIX)))
    }

    fn execute(&self, command: &CommandSpec) -> anyhow::Result<ProcessResult> {
        self.process.run(command).with_context(|| {
            format!(
                "{} did not start",
                Path::new(&command.program)
                    .file_name()
                    .unwrap_or(&command.program)
                    .to_string_lossy()
            )
        })
    }
}

#[cfg(test)]
#[path = "../tests/unit/tests.rs"]
mod tests;
