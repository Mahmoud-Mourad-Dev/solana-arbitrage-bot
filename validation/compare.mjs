// Semantic deep-compare of out-ts.json vs out-rs.json. Key order and
// integer-vs-float (123 vs 123.0) are irrelevant to correctness — what must
// match is the set of scenarios, and for each, the exact opportunities
// (ids, every amount, profit, bps, slot, and per-hop legs).
import { readFileSync } from "fs";
import { resolve, dirname } from "path";
import { fileURLToPath } from "url";

const dir = dirname(fileURLToPath(import.meta.url));
const ts = JSON.parse(readFileSync(resolve(dir, "out-ts.json"), "utf8"));
const rs = JSON.parse(readFileSync(resolve(dir, "out-rs.json"), "utf8"));

/** Canonicalize: sort object keys, coerce numbers to a common form. */
function canon(v) {
  if (Array.isArray(v)) return v.map(canon);
  if (v && typeof v === "object") {
    const out = {};
    for (const k of Object.keys(v).sort()) out[k] = canon(v[k]);
    return out;
  }
  if (typeof v === "number") return Number(v); // 123 === 123.0
  return v;
}

function deepEq(a, b, path) {
  const ca = JSON.stringify(canon(a));
  const cb = JSON.stringify(canon(b));
  if (ca !== cb) {
    console.error(`MISMATCH at ${path}:`);
    console.error(`  TS: ${ca}`);
    console.error(`  RS: ${cb}`);
    return false;
  }
  return true;
}

let ok = true;
const scenarios = new Set([...Object.keys(ts), ...Object.keys(rs)]);
let totalOpps = 0;

for (const name of [...scenarios].sort()) {
  if (!(name in ts)) { console.error(`scenario ${name} missing from TS`); ok = false; continue; }
  if (!(name in rs)) { console.error(`scenario ${name} missing from RS`); ok = false; continue; }
  const a = ts[name], b = rs[name];
  if (a.length !== b.length) {
    console.error(`scenario ${name}: TS has ${a.length} opps, RS has ${b.length}`);
    ok = false;
    continue;
  }
  totalOpps += a.length;
  for (let i = 0; i < a.length; i++) {
    if (!deepEq(a[i], b[i], `${name}[${i}]`)) ok = false;
  }
  console.log(`  ${ok ? "✓" : "✗"} ${name}: ${a.length} opportunit${a.length === 1 ? "y" : "ies"} match`);
}

if (ok) {
  console.log(`\n✅ PARITY PROVEN: ${scenarios.size} scenarios, ${totalOpps} opportunities — TS and Rust engines agree exactly.`);
  process.exit(0);
} else {
  console.error("\n❌ PARITY FAILED — see mismatches above.");
  process.exit(1);
}
