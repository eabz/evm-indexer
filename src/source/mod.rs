//! The data sources: the only places that talk to a HyperSync endpoint.
//!
//! One file per chain family, and nothing else - a source ingests, it
//! never decodes into rows (that is each data module's `decode`). The two
//! are SEPARATE client crates with different query languages and share no
//! code; what they share is the seam the pipeline sees: `head` /
//! `headers` / `stream`.

pub mod evm;
pub mod solana;
