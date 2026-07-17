//! arb-monitor — Rust replacement for the TypeScript price monitor.
//!
//! Phase B: full pipeline in Rust —
//!   Yellowstone Geyser gRPC -> parsers -> PoolRegistry (BigInt-safe via
//!   U256/U512 math) -> DiscoveryEngine (precomputed cycle index) -> Redis
//!   PUBLISH `arbitrage_opportunities`, producing the SAME JSON payload the
//!   TypeScript monitor emits (verified by differential tests).
//!
//! The TypeScript monitor in `src/` remains the production producer until
//! this crate is validated against live traffic side-by-side.

pub mod bootstrap;
pub mod config;
pub mod consistency;
pub mod discovery;
pub mod fixture_capture;
pub mod geyser;
pub mod market_discovery;
pub mod math;
pub mod meteora_direct_call;
pub mod meteora_dlmm;
pub mod meteora_reconstruct;
pub mod narrow_report;
pub mod observe_live;
pub mod observe_report;
pub mod optimizer;
pub mod parsers;
pub mod pipeline;
pub mod pump_amm;
pub mod pump_evidence;
pub mod pump_feev2;
pub mod pump_reconstruct;
pub mod pump_reprice;
pub mod quote;
pub mod redis_sink;
pub mod registry;
pub mod route_engine;
pub mod sim_client;
pub mod sim_parity;
pub mod tick_math;
pub mod types;
