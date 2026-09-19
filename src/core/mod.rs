//! The core EVM dataset: blocks, transactions, logs, withdrawals and the
//! ERC-20 / ERC-721 / ERC-1155 transfers decoded out of the logs.
//!
//! A DATA MODULE like `dex`, `predictions` and `launchpads`
//! (docs/design.md section 12): it owns its row structs, its event
//! signatures, its decoding, its table constants and its aggregates. See
//! `README.md` in this directory for the table catalogue.

pub mod convert;
pub mod decode;
pub mod events;
pub mod models;

pub use self::decode::decode;
