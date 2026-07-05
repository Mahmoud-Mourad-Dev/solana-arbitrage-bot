import { BufferReader } from "../utils";

/**
 * Raydium AMM v4 pool state (LIQUIDITY_STATE_LAYOUT_V4, 752 bytes).
 *
 * NOTE: Raydium v4 is a native (non-Anchor) program — there is NO 8-byte
 * Anchor discriminator. The account starts directly with `status: u64`.
 * Reserves are NOT stored in this account: effective reserves =
 * vault balance + openOrders totals − needTakePnl (see math.ts).
 */
export const RAYDIUM_V4_ACCOUNT_SIZE = 752;
export const RAYDIUM_V4_PROGRAM_ID = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";

export interface RaydiumV4Decoded {
  status: bigint;
  baseDecimal: number;
  quoteDecimal: number;
  tradeFeeNumerator: bigint;
  tradeFeeDenominator: bigint;
  swapFeeNumerator: bigint;
  swapFeeDenominator: bigint;
  baseNeedTakePnl: bigint;
  quoteNeedTakePnl: bigint;
  poolOpenTime: bigint;
  baseVault: string;
  quoteVault: string;
  baseMint: string;
  quoteMint: string;
  lpMint: string;
  openOrders: string;
  marketId: string;
  marketProgramId: string;
}

export function decodeRaydiumV4(data: Buffer): RaydiumV4Decoded | null {
  if (data.length !== RAYDIUM_V4_ACCOUNT_SIZE) return null;
  const r = new BufferReader(data);

  const status = r.u64();
  r.skip(8 * 3); // nonce, maxOrder, depth
  const baseDecimal = Number(r.u64());
  const quoteDecimal = Number(r.u64());
  r.skip(8 * 12); // state..maxPriceMultiplier, systemDecimalValue, minSeparate{Num,Den}
  const tradeFeeNumerator = r.u64();
  const tradeFeeDenominator = r.u64();
  r.skip(8 * 2); // pnlNumerator, pnlDenominator
  const swapFeeNumerator = r.u64();
  const swapFeeDenominator = r.u64();
  const baseNeedTakePnl = r.u64();
  const quoteNeedTakePnl = r.u64();
  r.skip(8 * 2); // quoteTotalPnl, baseTotalPnl
  const poolOpenTime = r.u64();
  r.skip(8 * 3); // punishPcAmount, punishCoinAmount, orderbookToInitTime
  r.skip(16 * 2 + 8 + 16 * 2 + 8); // swap volume counters (u128 x4, u64 x2)

  const baseVault = r.pubkey();
  const quoteVault = r.pubkey();
  const baseMint = r.pubkey();
  const quoteMint = r.pubkey();
  const lpMint = r.pubkey();
  const openOrders = r.pubkey();
  const marketId = r.pubkey();
  const marketProgramId = r.pubkey();
  // remaining: targetOrders, withdrawQueue, lpVault, owner, lpReserve, padding

  return {
    status,
    baseDecimal,
    quoteDecimal,
    tradeFeeNumerator,
    tradeFeeDenominator,
    swapFeeNumerator,
    swapFeeDenominator,
    baseNeedTakePnl,
    quoteNeedTakePnl,
    poolOpenTime,
    baseVault,
    quoteVault,
    baseMint,
    quoteMint,
    lpMint,
    openOrders,
    marketId,
    marketProgramId,
  };
}

/** AmmStatus values under which swapping is allowed. */
export function raydiumSwapEnabled(status: bigint, poolOpenTime: bigint, nowSec: bigint): boolean {
  // 1 = Initialized, 6 = SwapOnly, 7 = WaitingTrade (tradeable once open time passes)
  if (status === 1n || status === 6n) return true;
  if (status === 7n) return nowSec >= poolOpenTime;
  return false;
}
