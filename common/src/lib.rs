//! arb-common — single source of truth for everything both sides of the
//! wire must agree on:
//!
//! - [`ix`]: the on-chain instruction ABI (header/hop byte layout, DEX swap
//!   instruction data builders). Used by the program to PARSE and by the
//!   executor to ENCODE — drift is structurally impossible.
//! - [`opportunity`] (feature `serde`): the JSON payload published by the
//!   price monitor on the `arbitrage_opportunities` Redis channel.
//!
//! This crate deliberately has ZERO Solana dependencies so the future
//! Pinocchio (no_std-style) program can depend on it unchanged.

pub mod ix;

#[cfg(feature = "serde")]
pub mod opportunity;
