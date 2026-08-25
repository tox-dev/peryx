use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use clap::{CommandFactory as _, FromArgMatches as _};
use peryx_bench_core::compare;
use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::suite::{BenchmarkRun, BenchmarkSuite};
use peryx_bench_core::{machine, report};

#[derive(clap::Parser, Clone)]
struct Arguments {
    #[arg(long, default_value_t = 3)]
    rounds: usize,

    #[arg(long, value_name = "PART")]
    skip: Vec<String>,

    #[arg(long, default_value = "")]
    only: String,

    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    #[arg(long, value_name = "PATH")]
    scratch: Option<PathBuf>,

    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(Clone)]
struct Cli {
    suite: &'static dyn BenchmarkSuite,
    rounds: usize,
    skip: Vec<String>,
    only: String,
    report: Option<PathBuf>,
    scratch: Option<PathBuf>,
    mode: Option<Mode>,
    matches: clap::ArgMatches,
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

#[derive(Debug, PartialEq, Eq)]
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

#[derive(Debug, PartialEq, Eq)]
struct ProcessResult {
    success: bool,
    stdout: Vec<u8>,
}

trait Process: Send + Sync {
    fn run(&self, command: &CommandSpec) -> std::io::Result<ProcessResult>;
}

impl<F> Process for F
where
    F: Fn(&CommandSpec) -> std::io::Result<ProcessResult> + Send + Sync,
{
    fn run(&self, command: &CommandSpec) -> std::io::Result<ProcessResult> {
        self(command)
    }
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

impl<F> Comparator for F
where
    F: Fn(&Path, &Path) -> anyhow::Result<bool> + Send + Sync,
{
    fn compare(&self, base: &Path, head: &Path) -> anyhow::Result<bool> {
        self(base, head)
    }
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
            path: root.join("site/data/benchmark-machine.toml"),
            scratch: root.join(".tox/bench/scratch"),
            settings: machine::ProfileSettings::default(),
        }
    }

    async fn publish(&self, scratch: Option<&Path>) -> anyhow::Result<()> {
        let scratch = scratch.unwrap_or(&self.scratch);
        std::fs::create_dir_all(scratch)
            .with_context(|| format!("cannot create benchmark scratch at {}", scratch.display()))?;
        machine::write_profile(&self.path, scratch, self.settings).await
    }
}

pub struct Runner {
    core: Core,
}

struct Core {
    process: Box<dyn Process>,
    comparator: Box<dyn Comparator>,
    http: reqwest::Client,
    root: PathBuf,
    machine: MachineProfile,
}

impl Runner {
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

    /// # Errors
    /// Returns a parse or benchmark failure.
    pub async fn run_from<I, T>(&self, suite: &'static dyn BenchmarkSuite, args: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString> + Clone,
    {
        self.core.run(parse(suite, args)?).await
    }
}

fn parse<I, T>(suite: &'static dyn BenchmarkSuite, args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = suite.configure(Arguments::command()).try_get_matches_from(args)?;
    let arguments = Arguments::from_arg_matches(&matches)?;
    Ok(Cli {
        suite,
        rounds: arguments.rounds,
        skip: arguments.skip,
        only: arguments.only,
        report: arguments.report,
        scratch: arguments.scratch,
        mode: arguments.mode,
        matches,
    })
}

impl Core {
    fn new(
        process: impl Process + 'static,
        comparator: impl Comparator + 'static,
        root: PathBuf,
        machine: MachineProfile,
    ) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Self {
            process: Box::new(process),
            comparator: Box::new(comparator),
            http: reqwest::Client::new(),
            root,
            machine,
        }
    }

    async fn run(&self, cli: Cli) -> anyhow::Result<()> {
        if let Some(Mode::Ab { base, head_first }) = &cli.mode {
            return self.ab(base, *head_first, &cli).await;
        }
        let context = BenchmarkContext::with_scratch(
            self.ensure_peryx_built()?,
            cli.report
                .clone()
                .unwrap_or_else(|| self.root.join("site/data/bench/report.toml")),
            cli.scratch
                .clone()
                .unwrap_or_else(|| self.root.join(".tox/bench/scratch")),
        );
        self.run_suite(&context, &cli).await
    }

    async fn run_suite(&self, context: &BenchmarkContext, cli: &Cli) -> anyhow::Result<()> {
        std::fs::create_dir_all(context.scratch())?;
        if !cli.skip.iter().any(|part| part == "machine") {
            self.machine.publish(Some(context.scratch())).await?;
        }
        cli.suite
            .run(BenchmarkRun {
                context,
                rounds: cli.rounds,
                skip: &cli.skip,
                only: &cli.only,
                http: &self.http,
                matches: &cli.matches,
            })
            .await
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
        let scratch = cli
            .scratch
            .clone()
            .unwrap_or_else(|| self.root.join(".tox/bench/scratch"));
        let base_context = BenchmarkContext::with_scratch(base_binary, base_report.clone(), scratch.clone());
        let head_context = BenchmarkContext::with_scratch(head_binary, head_report.clone(), scratch);
        let measured = async {
            if head_first {
                self.measure("working tree", &head_context, &suite).await?;
                self.measure(&format!("base ({base_ref})"), &base_context, &suite).await
            } else {
                self.measure(&format!("base ({base_ref})"), &base_context, &suite)
                    .await?;
                self.measure("working tree", &head_context, &suite).await
            }
        }
        .await;
        let result = measured.and_then(|()| {
            if self.comparator.compare(&base_report, &head_report)? {
                bail!("peryx regressed against {base_ref}");
            }
            Ok(())
        });
        let cleanup = self.cleanup_comparison(&[base_report, head_report]);
        result.and(cleanup)
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
        match self.execute(&CommandSpec::status(
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
        )) {
            Ok(result) if result.success => {}
            Ok(_) => {
                self.remove_worktree().context("base build failed; cleanup failed")?;
                bail!("base build of {base_ref} failed");
            }
            Err(error) => {
                if let Err(cleanup) = self.remove_worktree() {
                    return Err(error.context(format!("worktree cleanup also failed: {cleanup:#}")));
                }
                return Err(error);
            }
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

    fn cleanup_comparison(&self, reports: &[PathBuf]) -> anyhow::Result<()> {
        let reports = reports
            .iter()
            .try_for_each(|report| match std::fs::remove_file(report) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error).with_context(|| format!("cannot remove {}", report.display())),
            });
        let worktree = self.remove_worktree();
        reports.and(worktree)
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
