/**
 * Core domain types for the price-monitor layer.
 * All on-chain balances are BigInt — never number — to avoid f64 rounding.
 */

export type DexKind = "raydium-v4" | "orca-whirlpool";

export interface TokenNode {
  /** Base58 mint address (graph node key). */
  mint: string;
  decimals: number;
  symbol?: string;
}

interface PoolStateBase {
  /** Base58 pool account address (graph edge key). */
  address: string;
  dex: DexKind;
  label?: string;
  /** Raydium: base/quote mint. Whirlpool: tokenMintA/tokenMintB. */
  mintA: string;
  mintB: string;
  vaultA: string;
  vaultB: string;
  decimalsA: number;
  decimalsB: number;
  /** Highest slot at which any constituent account was observed. */
  lastSlot: bigint;
  lastUpdatedMs: number;
  /** True once every account needed to quote this pool has been hydrated. */
  ready: boolean;
}

export interface RaydiumPoolState extends PoolStateBase {
  dex: "raydium-v4";
  /** SPL balances of the two vaults (raw units). */
  vaultABalance: bigint;
  vaultBBalance: bigint;
  /** Serum/OpenBook OpenOrders account owned by the AMM. */
  openOrders: string;
  openOrdersBaseTotal: bigint;
  openOrdersQuoteTotal: bigint;
  /** PnL owed to the protocol — must be subtracted from vault balances. */
  baseNeedTakePnl: bigint;
  quoteNeedTakePnl: bigint;
  /** Swap fee as parsed from the pool account (canonically 25/10000). */
  swapFeeNumerator: bigint;
  swapFeeDenominator: bigint;
  /** AmmStatus enum (6 = SwapOnly, 1 = Initialized, 7 = WaitingTrade). */
  status: bigint;
  /** Unix seconds after which a WaitingTrade pool becomes tradeable. */
  poolOpenTime: bigint;
}

export interface WhirlpoolState extends PoolStateBase {
  dex: "orca-whirlpool";
  /** Q64.64 fixed-point sqrt(price of B in A-terms). */
  sqrtPriceX64: bigint;
  /** Active in-range liquidity. */
  liquidity: bigint;
  tickCurrentIndex: number;
  tickSpacing: number;
  /** Fee rate in parts-per-million (e.g. 3000 = 30 bps). Dynamic per pool. */
  feeRatePpm: bigint;
  protocolFeeRate: number;
}

export type PoolState = RaydiumPoolState | WhirlpoolState;

/** One directed swap inside a cycle. */
export interface CycleHop {
  pool: string;
  dex: DexKind;
  inputMint: string;
  outputMint: string;
  amountIn: bigint;
  expectedAmountOut: bigint;
  /** expectedAmountOut minus SLIPPAGE_BPS — executor's on-chain floor. */
  minAmountOut: bigint;
}

export interface ArbitrageCycle {
  /** Stable id: short hash of the ordered pool route. */
  id: string;
  baseMint: string;
  baseSymbol?: string;
  hops: CycleHop[];
  amountIn: bigint;
  expectedAmountOut: bigint;
  /** expectedAmountOut - amountIn, in base-mint raw units. */
  grossProfit: bigint;
  /** Signature + priority fee + Jito tip, converted into base-mint units. */
  estimatedCostInBase: bigint;
  netProfit: bigint;
  netProfitBps: number;
  /** Slot of the freshest pool state used in the simulation. */
  slot: bigint;
  discoveredAtMs: number;
}

/** Static route (pool sequence anchored at a base mint), precomputed once. */
export interface CycleRoute {
  key: string;
  baseMint: string;
  /** Ordered pool addresses. */
  pools: string[];
  /** Ordered input mint per hop (output of hop i == input of hop i+1). */
  inputMints: string[];
}
