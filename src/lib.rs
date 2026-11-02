//! A compact Bitcask-style key-value store with segmented logs and compaction.
//! See README.md for persistence, recovery, and filesystem requirements.
#![forbid(unsafe_code)]

#[cfg(not(unix))]
compile_error!("bitcask-lite requires Unix directory syncing; use Linux, macOS, or WSL");

mod background;
mod codec;
mod store;

pub use codec::{MAX_KEY, MAX_VALUE};
pub use store::{Bitcask, Durability, Options, Stats};
