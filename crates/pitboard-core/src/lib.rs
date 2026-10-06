//! The engine behind Pitboard: parking and restoring a person's own Claude Code and Codex
//! logins, and reading what each has left. It serves Pitboard's own front ends, the command
//! line and the native apps, which reach it through [`service::Pitboard`] with an explicit
//! [`context::Context`].
//!
//! # What is supported
//!
//! This crate is published because the `pitboard` binary depends on it, not because it was
//! designed for other programs to build on. The supported interface is [`service::Pitboard`],
//! [`context::Context`], and the types those two return. Everything else is reachable so the
//! front ends in this repository can reach it, and may change in any release.
//!
//! What a version promises, for the part that is supported: a code is a name, and names are
//! kept. Adding an error, warning or check code is not a breaking change, which is why every
//! enum a caller reads codes out of is `#[non_exhaustive]` and every such caller needs a
//! fallback arm. Renaming or removing a code is a breaking change: a new minor version
//! while Pitboard is at 0.x, as Cargo reads one, and a new major version after 1.0.
//! A report a caller reads, such as what `uninstall` returns, may likewise say more in a
//! later release, so those structs are `#[non_exhaustive]` too.
//!
//! What is deliberately not reachable: nothing outside this crate may write Pitboard's index.
//! Every change goes through [`switch`], which records what it is about to do first and
//! finishes an interrupted one before starting another.

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!(
    "Pitboard runs on macOS and Linux. Another system needs a host of its own in \
     `host/`, saying where its stores, processes and scheduler are."
);

pub mod api;
pub mod app;
pub mod assumptions;
pub mod audit;
pub mod budget;
pub mod context;
pub mod desktop_code;
pub mod doctor;
pub mod error;
pub mod label;
pub mod provider;
pub mod redact;
pub mod schedule;
pub mod service;
pub mod settings;
pub mod state;
pub mod status;
pub mod statusline;
pub mod switch;
pub mod time;
pub mod usage;
pub mod words;

pub(crate) mod atomic;
pub(crate) mod fault;
pub mod history;
pub mod holder;
pub(crate) mod home;
pub mod host;
pub(crate) mod lock;
pub(crate) mod park;
pub(crate) mod pending;
pub(crate) mod readings;
pub(crate) mod sessions;
pub(crate) mod store;

/// What the integration tests reach into: they plant and inspect parked logins in the real
/// store, and must never touch the credential slot this machine's Claude Code reads.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod testing {
    pub use crate::api::scripted::{Answer, Asked, ScriptedApi, Trouble};
    pub use crate::host::memory::MemoryHost;
    pub use crate::provider::claude::paths::live_service;
    pub use crate::provider::claude::slot::{LIVE_SERVICE, dir_hash, service_for_dir};
    pub use crate::store::memory::{Fault, MemoryStore};
    pub use crate::store::{vault_delete, vault_read, vault_write};
    pub use crate::time::{Clock, FixedClock};

    /// Every variable Pitboard reads from its environment, by name, which a test withholds
    /// from every command it runs unless it means to pass one on.
    pub fn variables() -> impl Iterator<Item = &'static str> {
        crate::context::variables()
    }
}
