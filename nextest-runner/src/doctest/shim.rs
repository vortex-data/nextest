// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A libtest-compatible shim that runs doctests described by a manifest.

use super::{
    manifest::{DoctestAction, DoctestEntry, DoctestManifest},
    parse::{RUN_NB_TEST_ENV, SHOULD_PANIC_SUCCEEDED},
};
use crate::errors::DoctestError;
use camino::Utf8Path;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    io::{self, Write},
    process::Command,
};

/// The environment variable that makes a merged doctest runner run all tests,
/// spawning itself for each. It must not leak into the binary.
const BIN_PATH_ENV: &str = "RUSTDOC_DOCTEST_BIN_PATH";

/// The exit code libtest uses for test failures.
const FAILURE_EXIT_CODE: i32 = 101;

/// What the shim was asked to do.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ShimMode {
    List { ignored: bool },
    Run { name: String },
}

/// Parses the subset of libtest arguments that nextest passes to test binaries.
fn parse_args(args: &[String]) -> Result<ShimMode, DoctestError> {
    let mut list = false;
    let mut ignored = false;
    let mut exact = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--list" => list = true,
            "--ignored" => ignored = true,
            "--nocapture" | "--no-capture" | "--include-ignored" => {}
            "--format" => {
                let format = iter.next().map(String::as_str);
                if format != Some("terse") {
                    return Err(DoctestError::ShimArgs {
                        reason: format!("unsupported format {format:?}"),
                    });
                }
            }
            "--exact" => {
                let name = iter.next().ok_or_else(|| DoctestError::ShimArgs {
                    reason: "--exact requires a test name".to_owned(),
                })?;
                exact = Some(name.clone());
            }
            "--bench" => {
                return Err(DoctestError::ShimArgs {
                    reason: "doctests cannot be benchmarked".to_owned(),
                });
            }
            other => {
                return Err(DoctestError::ShimArgs {
                    reason: format!("unsupported argument `{other}`"),
                });
            }
        }
    }
    match (list, exact) {
        (true, None) => Ok(ShimMode::List { ignored }),
        (false, Some(name)) => Ok(ShimMode::Run { name }),
        (true, Some(_)) => Err(DoctestError::ShimArgs {
            reason: "--list and --exact are mutually exclusive".to_owned(),
        }),
        (false, None) => Err(DoctestError::ShimArgs {
            reason: "expected --list or --exact <name>".to_owned(),
        }),
    }
}

/// Runs the doctest shim. `args` are libtest-style arguments.
///
/// Returns the exit code to use.
pub fn run_shim(manifest_path: &Utf8Path, args: &[String]) -> Result<i32, DoctestError> {
    let mode = parse_args(args)?;
    let manifest = DoctestManifest::read(manifest_path)?;
    match mode {
        ShimMode::List { ignored } => {
            let mut stdout = io::stdout().lock();
            for test in &manifest.tests {
                // Like libtest, `--list` alone lists all tests, and
                // `--list --ignored` lists only ignored ones.
                if !ignored || test.ignored {
                    writeln!(stdout, "{}: test", test.name).map_err(stdout_err)?;
                }
            }
            stdout.flush().map_err(stdout_err)?;
            Ok(0)
        }
        ShimMode::Run { name } => {
            let test = manifest.get(&name).ok_or_else(|| DoctestError::ShimArgs {
                reason: format!("no doctest named `{name}` in `{manifest_path}`"),
            })?;
            run_test(test)
        }
    }
}

fn run_test(test: &DoctestEntry) -> Result<i32, DoctestError> {
    match &test.action {
        DoctestAction::Merged {
            binary,
            index,
            should_panic,
            no_run,
        } => {
            if *no_run {
                println!("doctest is marked `no_run`: it compiled, so it passed");
                return Ok(0);
            }
            let mut command = Command::new(binary);
            command
                .env(RUN_NB_TEST_ENV, index.to_string())
                .env_remove(BIN_PATH_ENV);
            run_binary(command, *should_panic)
        }
        DoctestAction::Standalone {
            binary,
            should_panic,
        } => {
            let mut command = Command::new(binary);
            command.env_remove(RUN_NB_TEST_ENV).env_remove(BIN_PATH_ENV);
            run_binary(command, *should_panic)
        }
        DoctestAction::Recorded { passed, output } => {
            if !output.is_empty() {
                println!("{output}");
            }
            Ok(if *passed { 0 } else { FAILURE_EXIT_CODE })
        }
        DoctestAction::Unavailable { reason } => {
            eprintln!("error: cannot run doctest `{}`: {reason}", test.name);
            Ok(FAILURE_EXIT_CODE)
        }
    }
}

fn run_binary(mut command: Command, should_panic: bool) -> Result<i32, DoctestError> {
    if !should_panic {
        // Replace the shim with the doctest on Unix, so that signals, exit
        // statuses, and resource usage are reported for the doctest itself.
        #[cfg(unix)]
        {
            let error = command.exec();
            return Err(spawn_err(&command, error));
        }
        #[cfg(not(unix))]
        {
            let status = command
                .status()
                .map_err(|error| spawn_err(&command, error))?;
            return Ok(status.code().unwrap_or(FAILURE_EXIT_CODE));
        }
    }

    // Matching rustdoc, a `should_panic` doctest passes if its process exits
    // unsuccessfully for any reason.
    let status = command
        .status()
        .map_err(|error| spawn_err(&command, error))?;
    if status.success() {
        println!("note: {}", SHOULD_PANIC_SUCCEEDED);
        Ok(FAILURE_EXIT_CODE)
    } else {
        Ok(0)
    }
}

fn spawn_err(command: &Command, error: io::Error) -> DoctestError {
    DoctestError::Io {
        path: command.get_program().to_string_lossy().into_owned().into(),
        error,
    }
}

fn stdout_err(error: io::Error) -> DoctestError {
    DoctestError::Io {
        path: "<stdout>".into(),
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_args() {
        let to_args = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        for (args, expected) in [
            (
                &["--list", "--format", "terse"][..],
                Some(ShimMode::List { ignored: false }),
            ),
            (
                &["--list", "--format", "terse", "--ignored"][..],
                Some(ShimMode::List { ignored: true }),
            ),
            (
                &["--exact", "src/lib.rs - a (line 1)", "--nocapture"][..],
                Some(ShimMode::Run {
                    name: "src/lib.rs - a (line 1)".to_owned(),
                }),
            ),
            (
                &["--exact", "x", "--nocapture", "--ignored"][..],
                Some(ShimMode::Run {
                    name: "x".to_owned(),
                }),
            ),
            (&["--exact", "x", "--bench"][..], None),
            (&["--list", "--format", "json"][..], None),
            (&["--list", "--exact", "x"][..], None),
            (&[][..], None),
        ] {
            assert_eq!(parse_args(&to_args(args)).ok(), expected, "args: {args:?}");
        }
    }
}
