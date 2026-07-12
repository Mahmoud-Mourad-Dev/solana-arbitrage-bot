//! Execution mode — the single most important safety switch in the bot.
//!
//! Four explicit modes, **default [`Mode::Observe`]**. Only [`Mode::Live`] may
//! ever submit a real transaction, and even then only when it has been armed
//! out-of-band (an explicit submit flag AND an on-disk acceptance marker). This
//! module is `no_std`-safe; the marker file-system check lives behind the
//! `std` feature so the on-chain program can depend on the enum unchanged.

use core::fmt;
use core::str::FromStr;

/// How the bot is allowed to act on what it discovers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Watch only: discover, quote, evaluate, report. NEVER build, sign, or
    /// submit a transaction. This is the default and the safe floor.
    #[default]
    Observe,
    /// Deterministic replay from recorded account state / real signatures.
    /// Answers "would we have found and priced this trade" — never submits.
    Replay,
    /// Build the real transaction and `simulateTransaction` it, comparing our
    /// local math against the simulated result. Never submits.
    Simulate,
    /// The ONLY mode permitted to send real transactions — and only after it
    /// has been armed (see [`live_armed`]).
    Live,
}

impl Mode {
    /// Stable lowercase identifier (round-trips with [`Mode::from_str`]).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Mode::Observe => "observe",
            Mode::Replay => "replay",
            Mode::Simulate => "simulate",
            Mode::Live => "live",
        }
    }

    /// The single predicate the rest of the codebase should branch on before
    /// sending anything. True for `Live` only.
    pub const fn allows_live_submission(&self) -> bool {
        matches!(self, Mode::Live)
    }

    /// `Live` demands explicit out-of-band arming; every other mode is inert.
    pub const fn requires_arming(&self) -> bool {
        matches!(self, Mode::Live)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when a mode string is unrecognised. Callers should treat a
/// parse failure as fatal rather than silently defaulting to a live path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeParseError;

impl fmt::Display for ModeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid mode (expected observe|replay|simulate|live)")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ModeParseError {}

impl FromStr for Mode {
    type Err = ModeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("observe") {
            Ok(Mode::Observe)
        } else if s.eq_ignore_ascii_case("replay") {
            Ok(Mode::Replay)
        } else if s.eq_ignore_ascii_case("simulate") {
            Ok(Mode::Simulate)
        } else if s.eq_ignore_ascii_case("live") {
            Ok(Mode::Live)
        } else {
            Err(ModeParseError)
        }
    }
}

/// The pure arming decision. Live may submit **only** when the operator has set
/// the explicit submit flag AND the acceptance marker is present. Every other
/// combination is inert. Kept `const` and side-effect-free so it is exhaustively
/// unit-testable without touching the filesystem.
pub const fn live_armed(submit_flag: bool, marker_present: bool) -> bool {
    submit_flag && marker_present
}

/// Whether the acceptance marker exists on disk. The marker is a deliberate,
/// manual gate: creating it is an operator's explicit statement that every
/// acceptance test has passed.
#[cfg(feature = "std")]
pub fn marker_present(path: &str) -> bool {
    std::path::Path::new(path).exists()
}

/// Resolve the effective mode. A requested `Live` mode is **refused** (returns
/// `Err`) unless it is fully armed — the caller must surface the error and stop,
/// never silently downgrade into an unexpected path.
#[cfg(feature = "std")]
pub fn resolve_live(
    requested: Mode,
    submit_flag: bool,
    marker_path: &str,
) -> Result<Mode, alloc::string::String> {
    use alloc::format;
    if requested != Mode::Live {
        return Ok(requested);
    }
    let present = marker_present(marker_path);
    if live_armed(submit_flag, present) {
        Ok(Mode::Live)
    } else {
        Err(format!(
            "MODE=live is NOT armed: submit_flag={submit_flag}, marker `{marker_path}` present={present}. \
             Live submission requires BOTH the explicit submit flag and the acceptance marker file."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_observe() {
        assert_eq!(Mode::default(), Mode::Observe);
    }

    #[test]
    fn only_live_submits() {
        assert!(!Mode::Observe.allows_live_submission());
        assert!(!Mode::Replay.allows_live_submission());
        assert!(!Mode::Simulate.allows_live_submission());
        assert!(Mode::Live.allows_live_submission());
    }

    #[test]
    fn parse_round_trips_and_is_case_insensitive() {
        for m in [Mode::Observe, Mode::Replay, Mode::Simulate, Mode::Live] {
            assert_eq!(Mode::from_str(m.as_str()).unwrap(), m);
        }
        assert_eq!(Mode::from_str("  LIVE ").unwrap(), Mode::Live);
        assert_eq!(Mode::from_str("Observe").unwrap(), Mode::Observe);
        assert!(Mode::from_str("").is_err());
        assert!(Mode::from_str("armed").is_err());
    }

    #[test]
    fn live_is_unreachable_without_both_flag_and_marker() {
        assert!(!live_armed(false, false));
        assert!(!live_armed(true, false)); // flag alone is not enough
        assert!(!live_armed(false, true)); // marker alone is not enough
        assert!(live_armed(true, true)); // both required
    }

    #[cfg(feature = "std")]
    #[test]
    fn resolve_live_refuses_unarmed_but_passes_other_modes() {
        // Non-live modes pass through untouched regardless of arming.
        assert_eq!(
            resolve_live(Mode::Observe, false, "/no/such/marker").unwrap(),
            Mode::Observe
        );
        // Live without a real marker is refused even with the flag set.
        assert!(resolve_live(Mode::Live, true, "/no/such/marker").is_err());
        assert!(resolve_live(Mode::Live, false, "/no/such/marker").is_err());
    }
}
