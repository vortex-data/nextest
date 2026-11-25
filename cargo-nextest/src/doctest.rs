// Copyright (c) The nextest Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Hidden entry points used to run doctests.
//!
//! See the `nextest_runner::doctest` module for an overview.

use camino::Utf8PathBuf;
use clap::Args;
use nextest_runner::{
    doctest::{DOCTEST_CAPTURE_DIR_ENV, record_rustdoc_invocation, run_runtool, run_shim},
    errors::{DisplayErrorChain, DoctestError},
};

/// Options for the runtool that rustdoc invokes with each compiled doctest.
#[derive(Debug, Args)]
pub(crate) struct DoctestRuntoolOpts {
    /// The directory to copy the binary into.
    crate_dir: Utf8PathBuf,

    /// The compiled doctest binary.
    binary: Utf8PathBuf,
}

impl DoctestRuntoolOpts {
    pub(crate) fn exec(self) -> i32 {
        report(run_runtool(&self.crate_dir, &self.binary).map(|()| 0))
    }
}

/// Options for the shim that runs doctests described by a manifest.
#[derive(Debug, Args)]
pub(crate) struct DoctestShimOpts {
    /// The doctest manifest.
    manifest: Utf8PathBuf,

    /// Arguments in the style of libtest.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl DoctestShimOpts {
    pub(crate) fn exec(self) -> i32 {
        report(run_shim(&self.manifest, &self.args))
    }
}

/// If nextest was invoked by Cargo in place of rustdoc, records the invocation
/// and exits.
pub(crate) fn maybe_act_as_rustdoc() {
    if let Some(dir) = std::env::var_os(DOCTEST_CAPTURE_DIR_ENV) {
        let code = match Utf8PathBuf::try_from(std::path::PathBuf::from(dir)) {
            Ok(dir) => report(record_rustdoc_invocation(&dir)),
            Err(err) => report(Err(DoctestError::NonUtf8 {
                what: "doctest capture directory",
                value: err.into_path_buf().to_string_lossy().into_owned(),
            })),
        };
        std::process::exit(code);
    }
}

fn report(result: Result<i32, DoctestError>) -> i32 {
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {}", DisplayErrorChain::new(&error));
            1
        }
    }
}
