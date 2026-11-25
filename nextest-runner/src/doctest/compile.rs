// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Compiling doctests ahead of time by replaying recorded rustdoc invocations.

use super::{
    capture::{DOCTEST_CAPTURE_DIR_ENV, DOCTEST_RUNTOOL_SUBCOMMAND, RustdocInvocation},
    manifest::{DOCTEST_MANIFEST_VERSION, DoctestAction, DoctestEntry, DoctestManifest},
    parse::{
        DISPLAY_SUFFIXES, LibtestStatus, SHOULD_PANIC_SUCCEEDED, current_test_from_output,
        parse_libtest_output, parse_merged_runner,
    },
};
use crate::errors::DoctestError;
use camino::{Utf8Path, Utf8PathBuf};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::ErrorKind,
    process::{Command, Output, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use tracing::{debug, warn};

/// The file that rustdoc's standard output is written to during compilation.
/// The runtool reads it to find out which standalone test is being run.
const STDOUT_LOG: &str = "rustdoc-stdout.log";

/// The file, within a captured binary's directory, holding the test name.
const NAME_FILE: &str = "name.txt";

/// The file, within a merged binary's directory, holding the runner source.
const RUNNER_FILE: &str = "runner.rs";

/// The result of compiling doctests for one crate: `None` if the crate has no
/// doctests.
type CompileResult = Result<Option<CompiledDoctests>, DoctestError>;

/// Compiles doctests for a set of crates, in parallel.
#[derive(Clone, Debug)]
pub struct DoctestCompiler {
    rustdoc: OsString,
    current_exe: Utf8PathBuf,
    out_dir: Utf8PathBuf,
    jobs: usize,
}

/// The doctests compiled for a single crate.
#[derive(Clone, Debug)]
pub struct CompiledDoctests {
    /// Where the manifest was written.
    pub manifest_path: Utf8PathBuf,

    /// The manifest.
    pub manifest: DoctestManifest,
}

impl DoctestCompiler {
    /// Creates a new compiler.
    ///
    /// * `rustdoc` is the rustdoc to run (see [`user_rustdoc`](super::user_rustdoc)).
    /// * `current_exe` is the nextest binary, which rustdoc calls back into as
    ///   its runtool.
    /// * `out_dir` is where compiled doctests and manifests are stored.
    /// * `jobs` is the number of crates to compile at once.
    pub fn new(
        rustdoc: OsString,
        current_exe: Utf8PathBuf,
        out_dir: Utf8PathBuf,
        jobs: usize,
    ) -> Self {
        Self {
            rustdoc,
            current_exe,
            out_dir,
            jobs: jobs.max(1),
        }
    }

    /// Compiles doctests for each recorded invocation. Crates without doctests
    /// are omitted from the result; the result is otherwise in input order.
    pub fn compile_all(
        &self,
        invocations: Vec<(Utf8PathBuf, RustdocInvocation)>,
    ) -> Result<Vec<CompiledDoctests>, DoctestError> {
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<Option<CompileResult>>> =
            Mutex::new((0..invocations.len()).map(|_| None).collect());

        std::thread::scope(|scope| {
            for _ in 0..self.jobs.min(invocations.len()) {
                scope.spawn(|| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some((path, invocation)) = invocations.get(index) else {
                            break;
                        };
                        let result = self.compile_one(path, invocation);
                        results.lock().expect("lock not poisoned")[index] = Some(result);
                    }
                });
            }
        });

        let mut compiled = Vec::new();
        for result in results.into_inner().expect("lock not poisoned") {
            if let Some(c) = result.expect("every invocation was processed")? {
                compiled.push(c);
            }
        }
        Ok(compiled)
    }

    fn compile_one(
        &self,
        invocation_path: &Utf8Path,
        invocation: &RustdocInvocation,
    ) -> CompileResult {
        let crate_name = invocation
            .crate_name()
            .ok_or_else(|| DoctestError::MissingArgument {
                path: invocation_path.to_owned(),
                arg: "--crate-name",
            })?
            .to_owned();
        let manifest_path =
            invocation
                .manifest_path()
                .ok_or_else(|| DoctestError::MissingArgument {
                    path: invocation_path.to_owned(),
                    arg: "CARGO_MANIFEST_DIR",
                })?;
        let base_args = base_args(&crate_name, &invocation.args)?;

        let crate_dir = self.out_dir.join(
            invocation_path
                .file_stem()
                .expect("invocation files are named <crate>-<pid>.json"),
        );
        recreate_dir(&crate_dir)?;

        // Compile the tests, handing each binary to the runtool. A single test
        // thread is what lets the runtool attribute standalone binaries to
        // tests.
        let log_path = crate_dir.join(STDOUT_LOG);
        let log = fs::File::create(&log_path).map_err(|error| DoctestError::Io {
            path: log_path.clone(),
            error,
        })?;
        let mut args = base_args.clone();
        args.extend(
            [
                "--test-args",
                "--test-threads=1",
                "--test-args",
                "--color=never",
                "--test-runtool",
                self.current_exe.as_str(),
                "--test-runtool-arg",
                DOCTEST_RUNTOOL_SUBCOMMAND,
                "--test-runtool-arg",
                crate_dir.as_str(),
            ]
            .map(str::to_owned),
        );
        // Put rustdoc's temporary directories next to where binaries are
        // stored, so the runtool can hard link binaries rather than copying
        // them.
        let tmp_dir = crate_dir.join("tmp");
        fs::create_dir(&tmp_dir).map_err(|error| DoctestError::Io {
            path: tmp_dir.clone(),
            error,
        })?;
        let mut command = self.command(&crate_name, invocation, &args, Stdio::from(log));
        for var in ["TMPDIR", "TMP", "TEMP"] {
            command.env(var, &tmp_dir);
        }
        let start = Instant::now();
        let output = self.output(&crate_name, command)?;
        debug!(
            "compiled doctests for {crate_name} in {:.2}s",
            start.elapsed().as_secs_f64()
        );
        let stdout = fs::read_to_string(&log_path).map_err(|error| DoctestError::Io {
            path: log_path.clone(),
            error,
        })?;
        // Rustdoc exits with 101 if any test "failed", which is expected: for
        // example, the runtool doesn't run `should_panic` tests, so rustdoc sees
        // them as having unexpectedly succeeded. But rustdoc also exits with 101
        // if it panics, so check that libtest actually reported a failure.
        let tests_failed =
            output.status.code() == Some(101) && stdout.contains("\ntest result: FAILED.");
        if !(output.status.success() || tests_failed) {
            return Err(DoctestError::RustdocFailed {
                action: "compiling",
                crate_name,
                status: output.status,
                stdout,
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }

        let tests = assemble(&crate_dir, &stdout)?;
        if tests.is_empty() {
            debug!("crate {crate_name} has no doctests");
            return Ok(None);
        }
        let manifest = DoctestManifest {
            version: DOCTEST_MANIFEST_VERSION,
            crate_name,
            manifest_path,
            tests,
        };
        let path = crate_dir.join("manifest.json");
        manifest.write(&path)?;
        Ok(Some(CompiledDoctests {
            manifest_path: path,
            manifest,
        }))
    }

    fn command(
        &self,
        crate_name: &str,
        invocation: &RustdocInvocation,
        args: &[String],
        stdout: Stdio,
    ) -> Command {
        let mut command = Command::new(&self.rustdoc);
        command
            .args(args)
            .current_dir(&invocation.cwd)
            .env_clear()
            .envs(&invocation.env)
            .env_remove(DOCTEST_CAPTURE_DIR_ENV)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped());
        debug!("rustdoc command for {crate_name}: {command:?}");
        command
    }

    fn output(&self, crate_name: &str, mut command: Command) -> Result<Output, DoctestError> {
        command
            .output()
            .map_err(|error| DoctestError::RustdocSpawn {
                program: self.rustdoc.to_string_lossy().into_owned(),
                crate_name: crate_name.to_owned(),
                error,
            })
    }
}

/// Removes arguments Cargo added that nextest needs to control, and rejects
/// arguments nextest doesn't support.
fn base_args(crate_name: &str, args: &[String]) -> Result<Vec<String>, DoctestError> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--test-args" {
            iter.next();
            continue;
        }
        if arg.starts_with("--test-args=") {
            continue;
        }
        if arg == "--test-runtool"
            || arg.starts_with("--test-runtool=")
            || arg == "--test-runtool-arg"
            || arg.starts_with("--test-runtool-arg=")
        {
            return Err(DoctestError::TargetRunnerUnsupported {
                crate_name: crate_name.to_owned(),
            });
        }
        out.push(arg.clone());
    }
    Ok(out)
}

fn recreate_dir(dir: &Utf8Path) -> Result<(), DoctestError> {
    match fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            return Err(DoctestError::Io {
                path: dir.to_owned(),
                error,
            });
        }
    }
    fs::create_dir_all(dir).map_err(|error| DoctestError::Io {
        path: dir.to_owned(),
        error,
    })
}

/// Converts a name as displayed by libtest, which may carry a suffix such as
/// ` - compile fail`, to the test's name.
///
/// Rustdoc names doctests `<path> - <item> (line <n>)`, so a name never ends
/// with one of these suffixes itself.
fn normalize_name(displayed: &str) -> &str {
    DISPLAY_SUFFIXES
        .iter()
        .find_map(|suffix| {
            displayed
                .strip_suffix(suffix)
                .filter(|name| name.ends_with(')'))
        })
        .unwrap_or(displayed)
}

/// Builds manifest entries from the binaries the runtool captured and the
/// libtest output rustdoc produced.
///
/// Every doctest is accounted for by exactly one of these: merged doctests
/// are described by the runner source of their merged binary, and rustdoc runs
/// all other doctests through libtest, which reports a result for each.
fn assemble(crate_dir: &Utf8Path, stdout: &str) -> Result<Vec<DoctestEntry>, DoctestError> {
    let results = parse_libtest_output(stdout);
    let mut statuses: BTreeMap<&str, (LibtestStatus, Option<&str>)> = BTreeMap::new();
    for (displayed, status) in &results.statuses {
        let output = results.failure_output.get(displayed).map(String::as_str);
        statuses.insert(normalize_name(displayed), (*status, output));
    }

    let io_err = |path: &Utf8Path| {
        let path = path.to_owned();
        move |error| DoctestError::Io { path, error }
    };
    let mut dirs: Vec<Utf8PathBuf> = Vec::new();
    for entry in crate_dir.read_dir_utf8().map_err(io_err(crate_dir))? {
        let entry = entry.map_err(io_err(crate_dir))?;
        let name = entry.file_name();
        if entry.path().is_dir() && (name.starts_with("merged-") || name.starts_with("standalone-"))
        {
            dirs.push(entry.into_path());
        }
    }
    dirs.sort();

    let mut entries: BTreeMap<String, DoctestEntry> = BTreeMap::new();
    for dir in dirs {
        let Some(binary) = find_binary(&dir) else {
            continue;
        };
        let runner_path = dir.join(RUNNER_FILE);
        if runner_path.exists() {
            let source = fs::read_to_string(&runner_path).map_err(io_err(&runner_path))?;
            let tests =
                parse_merged_runner(&source).map_err(|reason| DoctestError::RunnerParse {
                    path: runner_path.clone(),
                    reason,
                })?;
            for test in tests {
                if statuses.contains_key(test.name.as_str()) || entries.contains_key(&test.name) {
                    return Err(DoctestError::RunnerParse {
                        path: runner_path,
                        reason: format!(
                            "test `{}` was reported more than once by rustdoc",
                            test.name
                        ),
                    });
                }
                let action = DoctestAction::Merged {
                    binary: binary.clone(),
                    index: test.index,
                    should_panic: test.should_panic,
                    no_run: test.no_run,
                };
                entries.insert(
                    test.name.clone(),
                    DoctestEntry {
                        name: test.name,
                        ignored: test.ignore,
                        action,
                    },
                );
            }
        } else {
            let name_path = dir.join(NAME_FILE);
            let displayed = fs::read_to_string(&name_path).map_err(io_err(&name_path))?;
            let name = normalize_name(&displayed);
            // The runtool doesn't run binaries, so rustdoc reports
            // `should_panic` tests as having failed with a known message.
            let should_panic = match statuses.get(name) {
                Some((LibtestStatus::Failed, Some(output))) => {
                    output.contains(SHOULD_PANIC_SUCCEEDED)
                }
                Some(_) => false,
                None => {
                    warn!("rustdoc did not report a result for doctest `{name}`");
                    continue;
                }
            };
            entries.insert(
                name.to_owned(),
                DoctestEntry {
                    name: name.to_owned(),
                    ignored: false,
                    action: DoctestAction::Standalone {
                        binary,
                        should_panic,
                    },
                },
            );
        }
    }

    // Anything else that libtest reported was never handed to the runtool:
    // `compile_fail` and `no_run` tests, tests that failed to compile, and
    // ignored tests.
    for (name, (status, output)) in statuses {
        if entries.contains_key(name) {
            continue;
        }
        let (ignored, action) = match status {
            LibtestStatus::Ok => (
                false,
                DoctestAction::Recorded {
                    passed: true,
                    output: String::new(),
                },
            ),
            LibtestStatus::Failed => (
                false,
                DoctestAction::Recorded {
                    passed: false,
                    output: output.unwrap_or_default().to_owned(),
                },
            ),
            LibtestStatus::Ignored => (
                true,
                DoctestAction::Unavailable {
                    reason: "ignored doctests are not compiled, so nextest cannot run them; \
                             use `cargo test --doc -- --ignored` instead"
                        .to_owned(),
                },
            ),
        };
        entries.insert(
            name.to_owned(),
            DoctestEntry {
                name: name.to_owned(),
                ignored,
                action,
            },
        );
    }

    // BTreeMap iteration order is sorted by name, as the manifest requires.
    Ok(entries.into_values().collect())
}

fn find_binary(dir: &Utf8Path) -> Option<Utf8PathBuf> {
    ["rust_out", "rust_out.exe"]
        .into_iter()
        .map(|candidate| dir.join(candidate))
        .find(|path| path.is_file())
}

/// Acts as rustdoc's runtool: copies the compiled doctest binary into
/// `crate_dir` instead of running it.
pub fn run_runtool(crate_dir: &Utf8Path, binary: &Utf8Path) -> Result<(), DoctestError> {
    let io_err = |path: &Utf8Path| {
        let path = path.to_owned();
        move |error| DoctestError::Io { path, error }
    };
    let source_dir = binary.parent().ok_or_else(|| DoctestError::ShimArgs {
        reason: format!("runtool binary `{binary}` has no parent directory"),
    })?;
    let file_name = binary.file_name().ok_or_else(|| DoctestError::ShimArgs {
        reason: format!("runtool binary `{binary}` has no file name"),
    })?;

    // Merged doctests are compiled from a runner source file that rustdoc
    // writes next to the binary.
    let mut runner = None;
    for entry in source_dir.read_dir_utf8().map_err(io_err(source_dir))? {
        let entry = entry.map_err(io_err(source_dir))?;
        let name = entry.file_name();
        if name.starts_with("doctest_runner_") && name.ends_with(".rs") {
            runner = Some(entry.into_path());
            break;
        }
    }

    let (prefix, name) = match &runner {
        Some(_) => ("merged", None),
        None => {
            let log_path = crate_dir.join(STDOUT_LOG);
            let log = fs::read_to_string(&log_path).map_err(io_err(&log_path))?;
            let name = current_test_from_output(&log).ok_or_else(|| DoctestError::ShimArgs {
                reason: format!(
                    "could not determine the doctest being run from `{log_path}` \
                     (last line: {:?})",
                    log.rsplit('\n').next().unwrap_or_default()
                ),
            })?;
            ("standalone", Some(name.to_owned()))
        }
    };

    let dest = create_unique_dir(crate_dir, prefix)?;
    let dest_binary = dest.join(file_name);
    // Rustdoc deletes the binary once the runtool exits. Hard linking avoids
    // copying what can be hundreds of megabytes per crate, and works because
    // rustdoc's temporary directory is inside `crate_dir`. Fall back to
    // copying in case that ever isn't true.
    if fs::hard_link(binary, &dest_binary).is_err() {
        fs::copy(binary, &dest_binary).map_err(io_err(&dest_binary))?;
    }
    if let Some(runner) = runner {
        let dest_runner = dest.join(RUNNER_FILE);
        fs::copy(&runner, &dest_runner).map_err(io_err(&dest_runner))?;
    }
    if let Some(name) = name {
        let name_path = dest.join(NAME_FILE);
        fs::write(&name_path, name).map_err(io_err(&name_path))?;
    }
    Ok(())
}

fn create_unique_dir(parent: &Utf8Path, prefix: &str) -> Result<Utf8PathBuf, DoctestError> {
    for n in 0.. {
        let dir = parent.join(format!("{prefix}-{n}"));
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(DoctestError::Io { path: dir, error }),
        }
    }
    unreachable!("ran out of directory names")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_args_strip_and_reject() {
        let args: Vec<String> = [
            "--crate-name",
            "foo",
            "--test-args",
            "--list",
            "--test-args=--exact",
            "--test",
        ]
        .map(str::to_owned)
        .to_vec();
        assert_eq!(
            base_args("foo", &args).unwrap(),
            vec!["--crate-name", "foo", "--test"]
        );

        let with_runner: Vec<String> = ["--test", "--test-runtool", "qemu"]
            .map(str::to_owned)
            .to_vec();
        assert!(matches!(
            base_args("foo", &with_runner),
            Err(DoctestError::TargetRunnerUnsupported { .. })
        ));
    }

    #[test]
    fn normalize_display_names() {
        for (displayed, expected) in [
            ("src/lib.rs - a (line 1)", "src/lib.rs - a (line 1)"),
            (
                "src/lib.rs - a (line 1) - compile fail",
                "src/lib.rs - a (line 1)",
            ),
            (
                "src/lib.rs - a (line 1) - compile",
                "src/lib.rs - a (line 1)",
            ),
            (
                "src/lib.rs - a (line 1) - should panic",
                "src/lib.rs - a (line 1)",
            ),
            // Only a suffix that follows the line number is stripped.
            ("weird - compile", "weird - compile"),
        ] {
            assert_eq!(normalize_name(displayed), expected, "{displayed}");
        }
    }
}
