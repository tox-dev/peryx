use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser as _;

use crate::cli::{Cli, Command, JobCommand};

#[test]
fn test_documented_job_run_commands_parse() {
    let mut paths = markdown_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    paths.sort_unstable();

    let mut commands = 0;
    for path in paths {
        let markdown = fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        for (line, command) in documented_job_runs(&markdown) {
            commands += 1;
            let argv = shlex::split(&command)
                .unwrap_or_else(|| panic!("{}:{line}: invalid shell quoting: {command}", path.display()));
            Cli::try_parse_from(&argv)
                .unwrap_or_else(|error| panic!("{}:{line}: `{command}` does not parse: {error}", path.display()));
        }
    }
    assert_ne!(commands, 0, "no documented `peryx job run` commands found");
}

#[test]
fn test_documented_job_run_commands_support_shell_syntax() {
    let documented = documented_job_runs(
        "peryx job runner --target ignored\n$ peryx job run sync \\\n         --target 'private mirror'",
    );
    let parsed = Cli::try_parse_from(shlex::split(&documented[0].1).unwrap()).unwrap();
    assert!(matches!(
        (&documented[..], parsed.command),
        ([(2, _)], Command::Job(JobCommand::Run { command: Some(command), target, .. }))
            if command == "sync" && target == "private mirror"
    ));
}

fn markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut directories = vec![root.to_path_buf()];
    let mut paths = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory).unwrap_or_else(|error| panic!("{}: {error}", directory.display())) {
            let path = entry
                .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
                .path();
            if path.is_dir()
                && !matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(".git" | ".tox" | "node_modules" | "target")
                )
            {
                directories.push(path);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                paths.push(path);
            }
        }
    }
    paths
}

fn documented_job_runs(markdown: &str) -> Vec<(usize, String)> {
    let mut commands = Vec::new();
    let mut command = String::new();
    let mut start = 0;
    for (line, text) in markdown.lines().enumerate() {
        let text = text.trim_start();
        let text = text.strip_prefix("$ ").unwrap_or(text);
        if command.is_empty() {
            let Some(remainder) = text.strip_prefix("peryx job run") else {
                continue;
            };
            if remainder
                .chars()
                .next()
                .is_some_and(|character| !character.is_whitespace())
            {
                continue;
            }
            start = line + 1;
        }
        command.push_str(text);
        command.push('\n');
        if !text.trim_end().ends_with('\\') {
            commands.push((start, command.trim_end().to_owned()));
            command.clear();
        }
    }
    assert!(command.is_empty(), "line {start}: unterminated `peryx job run` command");
    commands
}
