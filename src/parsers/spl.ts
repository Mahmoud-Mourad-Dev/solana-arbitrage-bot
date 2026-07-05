/**
 * Minimal fixed-offset readers for SPL Token accounts / mints and
 * Serum-OpenBook OpenOrders accounts. Fixed offsets beat generic layout
 * libraries on the hot path (no allocation, no schema walk).
 */

export const SPL_TOKEN_ACCOUNT_SIZE = 165;

/** SPL token account: mint[0..32] owner[32..64] amount u64 LE [64..72]. */
export function decodeTokenAccountAmount(data: Buffer): bigint | null {
  if (data.length < 72) return null;
  return data.readBigUInt64LE(64);
}

/** SPL mint: decimals is a u8 at offset 44. */
export function decodeMintDecimals(data: Buffer): number | null {
  if (data.length < 45) return null;
  return data.readUInt8(44);
}

/**
 * Serum/OpenBook OpenOrders v2:
 * 5-byte "serum" head padding, accountFlags u64, market pk, owner pk,
 * baseTokenFree u64 @77, baseTokenTotal u64 @85,
 * quoteTokenFree u64 @93, quoteTokenTotal u64 @101.
 */
export interface OpenOrdersTotals {
  baseTokenTotal: bigint;
  quoteTokenTotal: bigint;
}

export function decodeOpenOrdersTotals(data: Buffer): OpenOrdersTotals | null {
  if (data.length < 109) return null;
  return {
    baseTokenTotal: data.readBigUInt64LE(85),
    quoteTokenTotal: data.readBigUInt64LE(101),
  };
}
