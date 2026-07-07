// Live side-by-side parity monitor.
//
// The TypeScript monitor and the Rust monitor run against the SAME Geyser
// feed and pool set, each publishing opportunities to a distinct Redis
// channel. This script subscribes to both and correlates by (id, slot):
// when BOTH engines emit the same cycle id at the same slot, they observed
// identical on-chain state, so every amount/profit/leg MUST match — any
// divergence there is a real bug. Opportunities only one side emits (within
// a time window) are reported as ts-only / rs-only (expected occasionally
// due to independent stream timing and per-engine cooldown, but a large
// asymmetry is a signal).
//
// Usage:
//   node validation/live/compare-live.mjs                # live (needs Redis)
//   node validation/live/compare-live.mjs --selftest     # offline logic check
//
// Env: REDIS_URL, TS_CHANNEL (default validation:ts), RS_CHANNEL (validation:rs),
//      WINDOW_MS (default 6000), REPORT_MS (default 15000)

const WINDOW_MS = Number(process.env.WINDOW_MS ?? 6000);

/** Canonical, comparison-ready form (discoveredAtMs excluded, amounts as
 *  strings, bps coerced to number). */
export function normalize(o) {
  const canon = (v) => {
    if (Array.isArray(v)) return v.map(canon);
    if (v && typeof v === "object") {
      const out = {};
      for (const k of Object.keys(v).sort()) out[k] = canon(v[k]);
      return out;
    }
    return v;
  };
  return JSON.stringify(canon({
    id: o.id,
    baseMint: o.baseMint,
    amountIn: String(o.amountIn),
    expectedAmountOut: String(o.expectedAmountOut),
    grossProfit: String(o.grossProfit),
    estimatedCostInBase: String(o.estimatedCostInBase),
    netProfit: String(o.netProfit),
    netProfitBps: Math.trunc(Number(o.netProfitBps)),
    slot: String(o.slot),
    hops: (o.hops ?? []).map((h) => ({
      pool: h.pool, dex: h.dex, inputMint: h.inputMint, outputMint: h.outputMint,
      amountIn: String(h.amountIn), expectedAmountOut: String(h.expectedAmountOut),
      minAmountOut: String(h.minAmountOut),
    })),
  }));
}

const keyOf = (o) => `${o.id}@${o.slot}`;

/** Correlates the two feeds. Pure (no I/O) so it is unit-testable. */
export class Correlator {
  constructor() {
    this.pending = new Map(); // key -> { ts?, rs?, at }
    this.stats = { matched: 0, divergent: 0, tsOnly: 0, rsOnly: 0 };
    this.divergences = [];
  }

  /** Ingest one opportunity from a side ("ts"|"rs"); returns an event or null. */
  ingest(side, opp, now = Date.now()) {
    const key = keyOf(opp);
    const norm = normalize(opp);
    const entry = this.pending.get(key) ?? { at: now };
    entry[side] = norm;
    entry.at = now;

    if (entry.ts !== undefined && entry.rs !== undefined) {
      this.pending.delete(key);
      if (entry.ts === entry.rs) {
        this.stats.matched++;
        return { kind: "match", key };
      }
      this.stats.divergent++;
      const d = { key, ts: entry.ts, rs: entry.rs };
      this.divergences.push(d);
      return { kind: "divergent", ...d };
    }
    this.pending.set(key, entry);
    return null;
  }

  /** Expire one-sided entries older than WINDOW_MS; updates tsOnly/rsOnly. */
  sweep(now = Date.now()) {
    const expired = [];
    for (const [key, e] of this.pending) {
      if (now - e.at < WINDOW_MS) continue;
      if (e.ts !== undefined) this.stats.tsOnly++;
      else this.stats.rsOnly++;
      expired.push([key, e.ts !== undefined ? "ts" : "rs"]);
      this.pending.delete(key);
    }
    return expired;
  }

  summary() {
    const { matched, divergent, tsOnly, rsOnly } = this.stats;
    const correlated = matched + divergent;
    const rate = correlated ? ((matched / correlated) * 100).toFixed(2) : "n/a";
    return { matched, divergent, tsOnly, rsOnly, correlated, matchRatePct: rate };
  }
}

// ── Offline self-test ────────────────────────────────────────────────────────

function selftest() {
  const c = new Correlator();
  const base = {
    id: "abc", slot: "100", baseMint: "So111", amountIn: "1000", expectedAmountOut: "1100",
    grossProfit: "100", estimatedCostInBase: "10", netProfit: "90", netProfitBps: 90,
    hops: [{ pool: "P", dex: "raydium-v4", inputMint: "A", outputMint: "B",
             amountIn: "1000", expectedAmountOut: "1100", minAmountOut: "1089" }],
  };
  const assert = (cond, msg) => { if (!cond) { console.error("SELFTEST FAIL:", msg); process.exit(1); } };

  // 1) identical id@slot, TS float bps vs RS int, different discoveredAtMs -> match
  assert(c.ingest("ts", { ...base, discoveredAtMs: 1 }) === null, "first side should buffer");
  const e1 = c.ingest("rs", { ...base, netProfitBps: 90.0, discoveredAtMs: 2 });
  assert(e1 && e1.kind === "match", "identical payloads must match across float/int bps + time");

  // 2) same id@slot but different amount -> divergent
  c.ingest("ts", { ...base, id: "x", slot: "200" });
  const e2 = c.ingest("rs", { ...base, id: "x", slot: "200", expectedAmountOut: "1101" });
  assert(e2 && e2.kind === "divergent", "differing amounts must be flagged divergent");

  // 3) same id different slot -> two separate one-sided (not correlated)
  c.ingest("ts", { ...base, id: "y", slot: "300" });
  c.ingest("rs", { ...base, id: "y", slot: "301" });
  const exp = c.sweep(Date.now() + WINDOW_MS + 1);
  assert(exp.length === 2, "different slots must not correlate; both expire one-sided");

  const s = c.summary();
  assert(s.matched === 1 && s.divergent === 1 && s.tsOnly === 1 && s.rsOnly === 1,
    `unexpected stats: ${JSON.stringify(s)}`);
  console.log("compare-live selftest OK —", JSON.stringify(s));
}

// ── Live mode ────────────────────────────────────────────────────────────────

async function live() {
  const { default: Redis } = await import("ioredis");
  const url = process.env.REDIS_URL ?? "redis://127.0.0.1:6379";
  const tsCh = process.env.TS_CHANNEL ?? "validation:ts";
  const rsCh = process.env.RS_CHANNEL ?? "validation:rs";
  const reportMs = Number(process.env.REPORT_MS ?? 15000);

  const sub = new Redis(url, { lazyConnect: true });
  await sub.connect();
  const corr = new Correlator();

  sub.on("message", (channel, payload) => {
    let opp;
    try { opp = JSON.parse(payload); } catch { return; }
    const side = channel === tsCh ? "ts" : "rs";
    const ev = corr.ingest(side, opp);
    if (ev?.kind === "divergent") {
      console.error(`\n❌ DIVERGENCE at ${ev.key}\n  TS: ${ev.ts}\n  RS: ${ev.rs}\n`);
    } else if (ev?.kind === "match") {
      process.stdout.write("✓");
    }
  });

  await sub.subscribe(tsCh, rsCh);
  console.log(`live parity: subscribed to '${tsCh}' (TS) and '${rsCh}' (RS) on ${url}`);
  console.log("waiting for opportunities… (✓ = correlated match, ❌ = divergence)\n");

  setInterval(() => {
    corr.sweep();
    const s = corr.summary();
    console.log(
      `\n[report] matched=${s.matched} divergent=${s.divergent} ` +
      `ts_only=${s.tsOnly} rs_only=${s.rsOnly} correlated=${s.correlated} ` +
      `match_rate=${s.matchRatePct}%`,
    );
    if (s.divergent > 0) console.log(`  ⚠ ${s.divergent} divergence(s) — engines disagree on identical state!`);
  }, reportMs);

  const shutdown = () => {
    const s = corr.summary();
    console.log(`\n\nFINAL: ${JSON.stringify(s)}`);
    process.exit(s.divergent > 0 ? 1 : 0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

if (process.argv.includes("--selftest")) {
  selftest();
} else {
  live().catch((e) => { console.error("fatal:", e); process.exit(1); });
}
