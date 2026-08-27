//! Venue-pair forensics (S15B generalized).
//!
//! READ-ONLY historical measurement of whether a venue pair carries an
//! arbitrage business: reachability (Q1), leader independence (Q2), land rate
//! (Q3) and realized economics (Q4) — from on-chain history only.
//!
//! Hard rules, inherited from `docs/forensics-s15b.md` and enforced here:
//! - No signing key is loaded in any code path in this module.
//! - Realized profit comes from balance deltas only — never from quoted or
//!   simulated prices.
//! - Every number carries its denominator.
//! - Missing data is an error or an explicit `Unsupported`, never an estimate.
//! - Silent pagination truncation is a hard error (`CensusError::Truncated`):
//!   it was one of the three measurement errors S15B caught in itself.

pub mod pipeline;
pub mod price;
pub mod schema;
pub mod venues;
