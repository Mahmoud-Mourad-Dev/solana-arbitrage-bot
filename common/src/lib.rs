//! arb-common — single source of truth for everything both sides of the
//! wire must agree on:
//!
//! - [`ix`]: the on-chain instruction ABI (header/hop byte layout, DEX swap
//!   instruction data builders). Used by the program to PARSE and by the
//!   executor to ENCODE — drift is structurally impossible.
//! - [`opportunity`] (feature `serde`): the JSON payload published by the
//!   price monitor on the `arbitrage_opportunities` Redis channel.
//!
//! This crate is `no_std` by default (only `alloc`) with ZERO Solana
//! dependencies, so the Pinocchio on-chain program can depend on it
//! unchanged. Enable the `std` feature (default) for off-chain use.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod cost;
pub mod ix;
pub mod mode;

#[cfg(feature = "serde")]
pub mod opportunity;
