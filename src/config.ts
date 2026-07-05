import { config as loadEnv } from "dotenv";
import { readFileSync } from "fs";
import { resolve } from "path";
import type { DexKind } from "./types";

loadEnv();

export const WSOL_MINT = "So11111111111111111111111111111111111111112";
export const USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

export const KNOWN_SYMBOLS: Record<string, string> = {
  [WSOL_MINT]: "SOL",
  [USDC_MINT]: "USDC",
  "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R": "RAY",
  "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB": "USDT",
};

export interface WatchedPoolConfig {
  address: string;
  dex: DexKind;
  label?: string;
}

export interface MonitorConfig {
  geyserEndpoint: string;
  geyserXToken: string | undefined;
  rpcEndpoint: string;

  redisUrl: string;
  opportunityChannel: string;
  opportunityList: string;
  opportunityListMax: number;
  redisFlushIntervalMs: number;
  redisFlushMaxCommands: number;

  baseMints: string[];
  maxHops: number;
  minProfitBps: number;
  slippageBps: number;
  maxClmmImpactBps: number;

  /** Input-amount optimizer bounds per base mint (raw units). */
  tradeBounds: Map<string, { min: bigint; max: bigint }>;

  baseSignatureFeeLamports: bigint;
  priorityFeeLamports: bigint;
  jitoTipLamports: bigint;

  opportunityCooldownMs: number;
  logLevel: "debug" | "info" | "warn" | "error";

  pools: WatchedPoolConfig[];
}

function envStr(name: string, fallback?: string): string {
  const v = process.env[name];
  if (v !== undefined && v !== "") return v;
  if (fallback !== undefined) return fallback;
  throw new Error(`Missing required env var: ${name}`);
}

function envInt(name: string, fallback: number): number {
  const v = process.env[name];
  if (v === undefined || v === "") return fallback;
  const n = Number(v);
  if (!Number.isFinite(n)) throw new Error(`Env var ${name} is not a number: ${v}`);
  return n;
}

function envBigInt(name: string, fallback: bigint): bigint {
  const v = process.env[name];
  if (v === undefined || v === "") return fallback;
  return BigInt(v);
}

export function loadConfig(requireGeyser = true): MonitorConfig {
  const poolsPath = resolve(__dirname, "..", "pools.json");
  const raw = JSON.parse(readFileSync(poolsPath, "utf8")) as { pools: WatchedPoolConfig[] };
  const pools = raw.pools.filter((p) => {
    const ok = p.address && (p.dex === "raydium-v4" || p.dex === "orca-whirlpool");
    if (!ok) console.warn(`[config] skipping invalid pool entry: ${JSON.stringify(p)}`);
    return ok;
  });

  const tradeBounds = new Map<string, { min: bigint; max: bigint }>([
    [WSOL_MINT, { min: envBigInt("TRADE_MIN_WSOL", 50_000_000n), max: envBigInt("TRADE_MAX_WSOL", 10_000_000_000n) }],
    [USDC_MINT, { min: envBigInt("TRADE_MIN_USDC", 5_000_000n), max: envBigInt("TRADE_MAX_USDC", 1_500_000_000n) }],
  ]);

  return {
    geyserEndpoint: requireGeyser ? envStr("GEYSER_ENDPOINT") : envStr("GEYSER_ENDPOINT", ""),
    geyserXToken: process.env.GEYSER_X_TOKEN || undefined,
    rpcEndpoint: envStr("RPC_ENDPOINT", "https://api.mainnet-beta.solana.com"),

    redisUrl: envStr("REDIS_URL", "redis://127.0.0.1:6379"),
    opportunityChannel: envStr("REDIS_OPPORTUNITY_CHANNEL", "arbitrage_opportunities"),
    opportunityList: envStr("REDIS_OPPORTUNITY_LIST", "arbitrage_opportunities"),
    opportunityListMax: envInt("REDIS_OPPORTUNITY_LIST_MAX", 1000),
    redisFlushIntervalMs: envInt("REDIS_FLUSH_INTERVAL_MS", 20),
    redisFlushMaxCommands: envInt("REDIS_FLUSH_MAX_COMMANDS", 256),

    baseMints: envStr("BASE_MINTS", `${WSOL_MINT},${USDC_MINT}`)
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean),
    maxHops: Math.min(Math.max(envInt("MAX_HOPS", 4), 2), 4),
    minProfitBps: envInt("MIN_PROFIT_BPS", 10),
    slippageBps: envInt("SLIPPAGE_BPS", 20),
    maxClmmImpactBps: envInt("MAX_CLMM_IMPACT_BPS", 100),

    tradeBounds,

    baseSignatureFeeLamports: envBigInt("BASE_SIGNATURE_FEE_LAMPORTS", 5_000n),
    priorityFeeLamports: envBigInt("PRIORITY_FEE_LAMPORTS", 100_000n),
    jitoTipLamports: envBigInt("JITO_TIP_LAMPORTS", 1_000_000n),

    opportunityCooldownMs: envInt("OPPORTUNITY_COOLDOWN_MS", 500),
    logLevel: (process.env.LOG_LEVEL as MonitorConfig["logLevel"]) || "info",

    pools,
  };
}
