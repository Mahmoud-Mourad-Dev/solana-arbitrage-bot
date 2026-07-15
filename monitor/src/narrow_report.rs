//! Narrow fast-poll report aggregation (PURE) — the single source of truth for
//! the corrected S13B metrics, used both live (`observe-narrow`) and offline
//! (`rebuild-report` on the poll JSONL), so they cannot diverge.
//!
//! Corrections vs the first narrow report:
//! - The HEADLINE economic figure is CAUSAL: value at first detection, then at
//!   the delayed reconfirmations. The in-episode maximum is kept only as an
//!   explicitly labelled `hindsight_upper_bound`.
//! - Route classification is derived from behaviour over the whole run:
//!   frozen/active TIME SHARES + a class that never brands a route that had
//!   real active episodes as "frozen" just because it later went quiet.
//! - `independently_active_routes` counts routes classed Active.
//!
//! Everything integer; costs are the caller's MODELED competitive figures.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One line of the narrow poll JSONL. `kind` is "poll" (default) or
/// "reconfirm". Reconfirm events carry `episode_start_ms` so they attach to the
/// exact episode that spawned them.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PollEvent {
    pub route: String,
    pub at_ms: u64,
    pub slot: u64,
    #[serde(default = "poll_kind")]
    pub kind: String,
    pub profitable_competitive: bool,
    pub gross_lamports: u64,
    pub competitive_net_lamports: i128,
    pub size_lamports: u64,
    /// Staleness fingerprint (round-trip gross at a fixed probe). i128::MIN if
    /// uncomputable — such polls never count toward frozen span.
    #[serde(default)]
    pub fingerprint: i128,
    #[serde(default)]
    pub snapshot_latency_ms: u64,
    /// For kind="reconfirm": ms from episode start to this reconfirm.
    #[serde(default)]
    pub reconfirm_delay_ms: Option<u64>,
    /// For kind="reconfirm": the episode it belongs to.
    #[serde(default)]
    pub episode_start_ms: Option<u64>,
}

fn poll_kind() -> String {
    "poll".to_string()
}

impl PollEvent {
    pub fn is_reconfirm(&self) -> bool {
        self.kind == "reconfirm"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RouteClass {
    /// Had at least one multi-poll competitive-positive episode.
    Active,
    /// Only single-poll flickers.
    Flicker,
    /// Never a competitive-positive poll, but stale (frozen) for a long span.
    FrozenSpread,
    /// Never profitable and never frozen.
    NeverProfitable,
}

#[derive(Debug, Clone, Serialize)]
pub struct EpisodeSummary {
    pub route: String,
    pub token: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub duration_ms: u64,
    pub polls: usize,
    /// CAUSAL: net at the first (detection) poll. Primary capturable proxy.
    pub net_at_detection_lamports: i128,
    pub size_at_detection_lamports: u64,
    pub gross_at_detection_lamports: u64,
    /// Reconfirm nets by milestone (None = episode ended before it / no sample).
    pub net_plus2s_lamports: Option<i128>,
    pub net_plus10s_lamports: Option<i128>,
    pub net_plus30s_lamports: Option<i128>,
    /// HINDSIGHT ONLY — the best net seen anywhere in the episode. NOT
    /// causally capturable at detection; upper bound for reference.
    pub hindsight_max_net_lamports: i128,
    pub distinct_gross_values: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RouteSummary {
    pub route: String,
    pub token: String,
    pub class: RouteClass,
    pub is_control: bool,
    pub polls: usize,
    /// Fraction of polls that were competitive-positive (in an episode).
    pub active_time_share: f64,
    /// Fraction of polls whose staleness fingerprint had been unchanged ≥
    /// frozen threshold at that moment.
    pub frozen_time_share: f64,
    pub episodes: usize,
    pub multi_poll_episodes: usize,
    /// Causal value contributed by this route (excl. control handling).
    pub causal_detection_lamports: i128,
}

#[derive(Debug, Clone, Serialize)]
pub struct NarrowMetrics {
    pub run_span_hours: f64,
    pub total_events: usize,
    pub poll_events: usize,
    pub reconfirm_events: usize,
    pub routes: usize,

    // Episodes.
    pub episodes_total: usize,
    pub episodes_per_day: f64,
    pub episode_lifetime_p50_ms: u64,
    pub episode_lifetime_p90_ms: u64,
    pub episode_lifetime_max_ms: u64,

    // Causal survival (reconfirm-based).
    pub survived_plus2s: usize,
    pub survived_plus10s: usize,
    pub survived_plus30s: usize,

    // HEADLINE causal economics (per day, controls excluded).
    pub causal_at_detection_per_day_lamports: i128,
    pub causal_plus2s_per_day_lamports: i128,
    pub causal_plus10s_per_day_lamports: i128,
    pub causal_plus30s_per_day_lamports: i128,
    /// Explicitly labelled non-capturable upper bound.
    pub hindsight_upper_bound_per_day_lamports: i128,

    // Classification.
    pub independently_active_routes: usize,
    pub class_active: usize,
    pub class_flicker: usize,
    pub class_frozen_spread: usize,
    pub class_never_profitable: usize,

    // Concentration.
    pub top3_causal_share_pct: u64,

    pub routes_detail: Vec<RouteSummary>,
    pub episodes_detail: Vec<EpisodeSummary>,
}

fn pctl_u64(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

/// Aggregate corrected narrow metrics from raw events.
/// `token_of` maps a route key's pump-pool prefix → token mint;
/// `control_tokens` are excluded from economic totals (kept as controls);
/// `frozen_secs` is the staleness threshold used for frozen time-share.
pub fn aggregate_narrow(
    events: &[PollEvent],
    token_of: &BTreeMap<String, String>,
    control_tokens: &[String],
    frozen_secs: u64,
) -> NarrowMetrics {
    let polls: Vec<&PollEvent> = events.iter().filter(|e| !e.is_reconfirm()).collect();
    let reconfirms: Vec<&PollEvent> = events.iter().filter(|e| e.is_reconfirm()).collect();

    let (t0, t1) = polls.iter().fold((u64::MAX, 0u64), |(lo, hi), e| {
        (lo.min(e.at_ms), hi.max(e.at_ms))
    });
    let span_hours = if t1 > t0 {
        (t1 - t0) as f64 / 3_600_000.0
    } else {
        1e-9
    };

    // Reconfirms grouped by (route, episode_start_ms).
    let mut rc: BTreeMap<(String, u64), Vec<&PollEvent>> = BTreeMap::new();
    for e in &reconfirms {
        if let Some(start) = e.episode_start_ms {
            rc.entry((e.route.clone(), start)).or_default().push(e);
        }
    }

    // Per route: time-ordered polls.
    let mut by_route: BTreeMap<String, Vec<&PollEvent>> = BTreeMap::new();
    for e in &polls {
        by_route.entry(e.route.clone()).or_default().push(e);
    }

    let is_control = |token: &str| control_tokens.iter().any(|c| c == token);
    let token_for = |route: &str| -> String {
        let pump = route.split('|').next().unwrap_or("");
        token_of.get(pump).cloned().unwrap_or_default()
    };

    let mut episodes: Vec<EpisodeSummary> = Vec::new();
    let mut route_summaries: Vec<RouteSummary> = Vec::new();

    for (route, mut ps) in by_route {
        ps.sort_by_key(|e| e.at_ms);
        let token = token_for(&route);
        let control = is_control(&token);

        // Frozen time-share: count polls whose fingerprint was unchanged for
        // ≥ frozen_secs at that moment.
        let mut frozen_polls = 0usize;
        let mut last_fp: Option<i128> = None;
        let mut identical_since: Option<u64> = None;
        for e in &ps {
            let identical = e.fingerprint != i128::MIN && last_fp == Some(e.fingerprint);
            if identical {
                let since = *identical_since.get_or_insert(e.at_ms);
                if e.at_ms.saturating_sub(since) >= frozen_secs * 1000 {
                    frozen_polls += 1;
                }
            } else {
                identical_since = Some(e.at_ms);
            }
            last_fp = Some(e.fingerprint);
        }

        // Episodes = maximal runs of consecutive profitable polls.
        let mut ep_count = 0usize;
        let mut multi = 0usize;
        let mut active_polls = 0usize;
        let mut cur: Vec<&PollEvent> = Vec::new();
        let mut route_causal: i128 = 0;
        let flush = |cur: &mut Vec<&PollEvent>,
                     episodes: &mut Vec<EpisodeSummary>,
                     ep_count: &mut usize,
                     multi: &mut usize,
                     route_causal: &mut i128| {
            if cur.is_empty() {
                return;
            }
            *ep_count += 1;
            if cur.len() > 1 {
                *multi += 1;
            }
            let start = cur[0].at_ms;
            let end = cur.last().unwrap().at_ms;
            let nets: Vec<i128> = cur.iter().map(|e| e.competitive_net_lamports).collect();
            let mut distinct: Vec<u64> = cur.iter().map(|e| e.gross_lamports).collect();
            distinct.sort_unstable();
            distinct.dedup();
            // Reconfirm milestones for this episode.
            let key = (cur[0].route.clone(), start);
            let milestone = |lo: u64, hi: u64| -> Option<i128> {
                rc.get(&key).and_then(|v| {
                    v.iter()
                        .filter(|e| {
                            let d = e.reconfirm_delay_ms.unwrap_or(0);
                            d >= lo && d < hi
                        })
                        .map(|e| e.competitive_net_lamports)
                        .next()
                })
            };
            let det = nets[0];
            *route_causal += det.max(0);
            episodes.push(EpisodeSummary {
                route: cur[0].route.clone(),
                token: String::new(), // filled below
                start_ms: start,
                end_ms: end,
                duration_ms: end - start,
                polls: cur.len(),
                net_at_detection_lamports: det,
                size_at_detection_lamports: cur[0].size_lamports,
                gross_at_detection_lamports: cur[0].gross_lamports,
                net_plus2s_lamports: milestone(1_000, 6_000),
                net_plus10s_lamports: milestone(6_000, 20_000),
                net_plus30s_lamports: milestone(20_000, 120_000),
                hindsight_max_net_lamports: nets.iter().copied().max().unwrap_or(det),
                distinct_gross_values: distinct.len(),
            });
            cur.clear();
        };
        for e in &ps {
            if e.profitable_competitive {
                active_polls += 1;
                cur.push(e);
            } else {
                flush(
                    &mut cur,
                    &mut episodes,
                    &mut ep_count,
                    &mut multi,
                    &mut route_causal,
                );
            }
        }
        flush(
            &mut cur,
            &mut episodes,
            &mut ep_count,
            &mut multi,
            &mut route_causal,
        );

        let n = ps.len().max(1);
        let class = if multi > 0 {
            RouteClass::Active
        } else if ep_count > 0 {
            RouteClass::Flicker
        } else if frozen_polls > 0 {
            RouteClass::FrozenSpread
        } else {
            RouteClass::NeverProfitable
        };
        route_summaries.push(RouteSummary {
            route: route.clone(),
            token: token.clone(),
            class,
            is_control: control,
            polls: ps.len(),
            active_time_share: active_polls as f64 / n as f64,
            frozen_time_share: frozen_polls as f64 / n as f64,
            episodes: ep_count,
            multi_poll_episodes: multi,
            causal_detection_lamports: if control { 0 } else { route_causal },
        });
    }

    // Fill episode tokens.
    for e in &mut episodes {
        e.token = token_for(&e.route);
    }

    // Causal daily economics (controls excluded).
    let per_day = |sum: i128| -> i128 { (sum as f64 / span_hours * 24.0) as i128 };
    let sum_if = |f: &dyn Fn(&EpisodeSummary) -> Option<i128>| -> i128 {
        episodes
            .iter()
            .filter(|e| !is_control(&e.token))
            .map(|e| f(e).unwrap_or(0).max(0))
            .sum()
    };
    let causal_detect = sum_if(&|e| Some(e.net_at_detection_lamports));
    let causal_p2 = sum_if(&|e| e.net_plus2s_lamports);
    let causal_p10 = sum_if(&|e| e.net_plus10s_lamports);
    let causal_p30 = sum_if(&|e| e.net_plus30s_lamports);
    let hindsight = sum_if(&|e| Some(e.hindsight_max_net_lamports));

    // Survival counts (episodes with a surviving reconfirm in each window).
    let survived = |lo: u64, hi: u64| -> usize {
        episodes
            .iter()
            .filter(|e| {
                let m = match (lo, hi) {
                    (1_000, 6_000) => e.net_plus2s_lamports,
                    (6_000, 20_000) => e.net_plus10s_lamports,
                    _ => e.net_plus30s_lamports,
                };
                m.map(|n| n >= 0).unwrap_or(false)
            })
            .count()
    };

    let mut durs: Vec<u64> = episodes.iter().map(|e| e.duration_ms).collect();
    durs.sort_unstable();

    // Concentration: top-3 routes by causal detection value.
    let mut route_vals: Vec<i128> = route_summaries
        .iter()
        .filter(|r| !r.is_control)
        .map(|r| r.causal_detection_lamports)
        .collect();
    route_vals.sort_unstable_by(|a, b| b.cmp(a));
    let total_causal: i128 = route_vals.iter().sum();
    let top3: i128 = route_vals.iter().take(3).sum();
    let top3_pct = if total_causal > 0 {
        (top3 * 100 / total_causal) as u64
    } else {
        0
    };

    let count_class = |c: RouteClass| route_summaries.iter().filter(|r| r.class == c).count();

    NarrowMetrics {
        run_span_hours: span_hours,
        total_events: events.len(),
        poll_events: polls.len(),
        reconfirm_events: reconfirms.len(),
        routes: route_summaries.len(),
        episodes_total: episodes.len(),
        episodes_per_day: episodes.len() as f64 / span_hours * 24.0,
        episode_lifetime_p50_ms: pctl_u64(&durs, 0.5),
        episode_lifetime_p90_ms: pctl_u64(&durs, 0.9),
        episode_lifetime_max_ms: durs.last().copied().unwrap_or(0),
        survived_plus2s: survived(1_000, 6_000),
        survived_plus10s: survived(6_000, 20_000),
        survived_plus30s: survived(20_000, 120_000),
        causal_at_detection_per_day_lamports: per_day(causal_detect),
        causal_plus2s_per_day_lamports: per_day(causal_p2),
        causal_plus10s_per_day_lamports: per_day(causal_p10),
        causal_plus30s_per_day_lamports: per_day(causal_p30),
        hindsight_upper_bound_per_day_lamports: per_day(hindsight),
        independently_active_routes: count_class(RouteClass::Active),
        class_active: count_class(RouteClass::Active),
        class_flicker: count_class(RouteClass::Flicker),
        class_frozen_spread: count_class(RouteClass::FrozenSpread),
        class_never_profitable: count_class(RouteClass::NeverProfitable),
        top3_causal_share_pct: top3_pct,
        routes_detail: route_summaries,
        episodes_detail: episodes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poll(route: &str, at: u64, prof: bool, net: i128, fp: i128) -> PollEvent {
        PollEvent {
            route: route.into(),
            at_ms: at,
            slot: 100,
            kind: "poll".into(),
            profitable_competitive: prof,
            gross_lamports: if prof { 3_000_000 } else { 0 },
            competitive_net_lamports: net,
            size_lamports: 1_000_000_000,
            fingerprint: fp,
            snapshot_latency_ms: 40,
            reconfirm_delay_ms: None,
            episode_start_ms: None,
        }
    }
    fn recon(route: &str, start: u64, delay: u64, net: i128) -> PollEvent {
        PollEvent {
            route: route.into(),
            at_ms: start + delay,
            slot: 101,
            kind: "reconfirm".into(),
            profitable_competitive: net >= 0,
            gross_lamports: 3_000_000,
            competitive_net_lamports: net,
            size_lamports: 1_000_000_000,
            fingerprint: 0,
            snapshot_latency_ms: 40,
            reconfirm_delay_ms: Some(delay),
            episode_start_ms: Some(start),
        }
    }

    fn tokmap() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("PA".into(), "TOKA".into());
        m.insert("PC".into(), "CTRL".into());
        m
    }

    #[test]
    fn headline_is_causal_not_hindsight() {
        // One episode: detection net 100, then rises to 900 mid-episode.
        // Hindsight would claim 900; causal-at-detection is 100.
        let ra = "PA|DA|meteora->pump";
        let events = vec![
            poll(ra, 0, true, 100, 10),
            poll(ra, 4_000, true, 900, 11),
            poll(ra, 8_000, false, -5, 12),
        ];
        let m = aggregate_narrow(&events, &tokmap(), &[], 600);
        assert_eq!(m.episodes_total, 1);
        assert_eq!(m.episodes_detail[0].net_at_detection_lamports, 100);
        assert_eq!(m.episodes_detail[0].hindsight_max_net_lamports, 900);
        // Daily headline uses detection (100), not hindsight (900).
        assert!(m.causal_at_detection_per_day_lamports < m.hindsight_upper_bound_per_day_lamports);
    }

    #[test]
    fn active_route_is_not_branded_frozen_after_going_quiet() {
        // Route has a real 2-poll episode, THEN a long identical (frozen) tail.
        let ra = "PA|DA|meteora->pump";
        let mut events = vec![
            poll(ra, 0, true, 200, 10),
            poll(ra, 4_000, true, 180, 11),
            poll(ra, 8_000, false, -5, 12),
        ];
        // 20 min of identical fingerprint, non-profitable.
        for i in 0..300 {
            events.push(poll(ra, 12_000 + i * 4_000, false, -5, 42));
        }
        let m = aggregate_narrow(&events, &tokmap(), &[], 600);
        let r = &m.routes_detail[0];
        assert_eq!(r.class, RouteClass::Active, "had a real episode ⇒ Active");
        // Frozen only counts AFTER the 600s threshold elapses within the
        // identical run, so a large but not-total share is expected.
        assert!(r.frozen_time_share > 0.4, "share={}", r.frozen_time_share);
        assert!(r.active_time_share < 0.05);
        assert_eq!(m.independently_active_routes, 1);
    }

    #[test]
    fn control_excluded_reconfirms_matched_survival_counted() {
        let ra = "PA|DA|meteora->pump";
        let rc = "PC|DC|meteora->pump";
        let events = vec![
            // active route episode with reconfirms surviving +2s/+10s, dying +30s
            poll(ra, 0, true, 500, 10),
            poll(ra, 4_000, true, 480, 11),
            poll(ra, 8_000, false, -5, 12),
            recon(ra, 0, 2_000, 470),
            recon(ra, 0, 10_000, 300),
            recon(ra, 0, 30_000, -50),
            // control route "profitable" — must NOT add to economics.
            poll(rc, 0, true, 1_000_000, 10),
            poll(rc, 4_000, false, -5, 10),
        ];
        let m = aggregate_narrow(&events, &tokmap(), &["CTRL".into()], 600);
        assert_eq!(m.survived_plus2s, 1);
        assert_eq!(m.survived_plus10s, 1);
        assert_eq!(m.survived_plus30s, 0); // died at +30
                                           // Control's huge "profit" excluded ⇒ detection economics come from TOKA
                                           // only (500 lamports once, scaled to per-day).
        let e = m
            .episodes_detail
            .iter()
            .find(|e| e.token == "TOKA")
            .unwrap();
        assert_eq!(e.net_plus2s_lamports, Some(470));
        assert_eq!(e.net_plus30s_lamports, Some(-50));
        // control contributes 0 causal value.
        let ctrl = m.routes_detail.iter().find(|r| r.is_control).unwrap();
        assert_eq!(ctrl.causal_detection_lamports, 0);
    }
}
