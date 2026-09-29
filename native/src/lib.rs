// SPDX-License-Identifier: GPL-3.0-or-later

/// The package version, shown in the UI and used to decide whether the
/// release notes for this build have already been seen.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod config;
pub mod coordinator;
pub mod domain;
pub mod media_jobs;
pub mod poe2;
pub mod process;
pub mod recorder;
pub mod storage;
