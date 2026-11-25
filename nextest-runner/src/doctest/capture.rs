// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Recording the rustdoc invocations that Cargo makes for doctests.

use crate::errors::DoctestError;
use camino::{Utf8Path, Utf8PathBuf};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, env, ffi::OsString, fs};

/// The environment variable that, when set, makes the nextest binary act as a
/// rustdoc wrapper that records doctest invocations into the given directory.
pub const DOCTEST_CAPTURE_DIR_ENV: &str = "__NEXTEST_DOCTEST_CAPTURE_DIR";

/// The environment variable that holds the rustdoc that nextest should use
/// after capture, if the user had `RUSTDOC` set.
pub const DOCTEST_REAL_RUSTDOC_ENV: &str = "__NEXTEST_DOCTEST_REAL_RUSTDOC";

/// The name of the hidden subcommand that rustdoc invokes as its runtool.
pub const DOCTEST_RUNTOOL_SUBCOMMAND: &str = "__doctest-runtool";

/// The name of the hidden subcommand that runs a single doctest from a manifest.
pub const DOCTEST_SHIM_SUBCOMMAND: &str = "__doctest-shim";

/// A rustdoc invocation recorded from Cargo.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RustdocInvocation {
    /// The arguments passed to rustdoc (not including the program name).
    pub args: Vec<String>,

    /// The working directory rustdoc was invoked in.
    pub cwd: Utf8PathBuf,

    /// The full environment rustdoc was invoked with.
    pub env: BTreeMap<String, String>,
}

impl RustdocInvocation {
    /// Returns the value of the crate name argument, if present.
    pub fn crate_name(&self) -> Option<&str> {
        self.arg_value("--crate-name")
    }

    /// Returns the value of a `--flag value` or `--flag=value` style argument.
    pub fn arg_value(&self, flag: &str) -> Option<&str> {
        let mut iter = self.args.iter();
        while let Some(arg) = iter.next() {
            if arg == flag {
                return iter.next().map(String::as_str);
            }
            if let Some(value) = arg.strip_prefix(flag).and_then(|v| v.strip_prefix('=')) {
                return Some(value);
            }
        }
        None
    }

    /// Returns the path to the package manifest, based on the environment
    /// Cargo set for rustdoc.
    pub fn manifest_path(&self) -> Option<Utf8PathBuf> {
        if let Some(path) = self.env.get("CARGO_MANIFEST_PATH") {
            return Some(path.into());
        }
        self.env
            .get("CARGO_MANIFEST_DIR")
            .map(|dir| Utf8Path::new(dir).join("Cargo.toml"))
    }
}

/// Acts as rustdoc for Cargo: records doctest invocations and forwards
/// everything else to the real rustdoc.
///
/// Returns the exit code to use.
pub fn record_rustdoc_invocation(capture_dir: &Utf8Path) -> Result<i32, DoctestError> {
    let args = env::args_os()
        .skip(1)
        .map(|arg| into_utf8(arg, "rustdoc argument"))
        .collect::<Result<Vec<_>, _>>()?;

    if !args.iter().any(|arg| arg == "--test") {
        // Not a doctest invocation (for example a version probe): behave like
        // rustdoc.
        let real = real_rustdoc();
        let status = std::process::Command::new(&real)
            .args(&args)
            .env_remove(DOCTEST_CAPTURE_DIR_ENV)
            .status()
            .map_err(|error| DoctestError::RustdocSpawn {
                program: real.to_string_lossy().into_owned(),
                crate_name: "(unknown)".to_owned(),
                error,
            })?;
        return Ok(status.code().unwrap_or(1));
    }

    let cwd = env::current_dir().map_err(|error| DoctestError::Io {
        path: ".".into(),
        error,
    })?;
    let cwd = Utf8PathBuf::try_from(cwd).map_err(|err| DoctestError::NonUtf8 {
        what: "current directory",
        value: err.into_path_buf().to_string_lossy().into_owned(),
    })?;

    let mut env_map = BTreeMap::new();
    for (key, value) in env::vars_os() {
        let key = into_utf8(key, "environment variable name")?;
        if key == DOCTEST_CAPTURE_DIR_ENV || key == "RUSTDOC" {
            continue;
        }
        let value = into_utf8(value, "environment variable value")?;
        env_map.insert(key, value);
    }

    let invocation = RustdocInvocation {
        args,
        cwd,
        env: env_map,
    };
    let crate_name = invocation.crate_name().unwrap_or("unknown");
    let path = capture_dir.join(format!("{crate_name}-{}.json", std::process::id()));
    let json = serde_json::to_vec_pretty(&invocation).map_err(|error| DoctestError::Json {
        path: path.clone(),
        error,
    })?;
    fs::write(&path, json).map_err(|error| DoctestError::Io { path, error })?;
    Ok(0)
}

/// Reads all invocations recorded into `capture_dir`, sorted by file name.
pub fn read_rustdoc_invocations(
    capture_dir: &Utf8Path,
) -> Result<Vec<(Utf8PathBuf, RustdocInvocation)>, DoctestError> {
    let io_err = |error| DoctestError::Io {
        path: capture_dir.to_owned(),
        error,
    };
    let mut paths = Vec::new();
    for entry in capture_dir.read_dir_utf8().map_err(io_err)? {
        let entry = entry.map_err(io_err)?;
        if entry.path().extension() == Some("json") {
            paths.push(entry.into_path());
        }
    }
    paths.sort();

    paths
        .into_iter()
        .map(|path| {
            let contents = fs::read(&path).map_err(|error| DoctestError::Io {
                path: path.clone(),
                error,
            })?;
            let invocation =
                serde_json::from_slice(&contents).map_err(|error| DoctestError::Json {
                    path: path.clone(),
                    error,
                })?;
            Ok((path, invocation))
        })
        .collect()
}

/// Returns the rustdoc to forward to while acting as the rustdoc wrapper.
fn real_rustdoc() -> OsString {
    env::var_os(DOCTEST_REAL_RUSTDOC_ENV)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "rustdoc".into())
}

/// Returns the user's rustdoc, to be used for compiling doctests and to be
/// passed to the wrapper via [`DOCTEST_REAL_RUSTDOC_ENV`].
///
/// This is the user's `RUSTDOC` if set, and otherwise `rustdoc` from `PATH`.
/// Rustup's proxy then picks the same toolchain Cargo did, since the recorded
/// environment includes `RUSTUP_TOOLCHAIN` and `PATH`.
pub fn user_rustdoc() -> OsString {
    env::var_os("RUSTDOC")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "rustdoc".into())
}

fn into_utf8(value: OsString, what: &'static str) -> Result<String, DoctestError> {
    value.into_string().map_err(|value| DoctestError::NonUtf8 {
        what,
        value: value.to_string_lossy().into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_value_forms() {
        let invocation = RustdocInvocation {
            args: vec![
                "--edition=2024".to_owned(),
                "--crate-name".to_owned(),
                "foo".to_owned(),
                "--test".to_owned(),
            ],
            cwd: "/".into(),
            env: BTreeMap::new(),
        };
        assert_eq!(invocation.crate_name(), Some("foo"));
        assert_eq!(invocation.arg_value("--edition"), Some("2024"));
        assert_eq!(invocation.arg_value("--test-run-directory"), None);
    }
}
