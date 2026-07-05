import { BufferReader } from "../utils";

/**
 * Orca Whirlpool account (Anchor program — 8-byte discriminator, 653 bytes).
 * We parse the fields required to quote swaps within the current tick:
 * sqrtPrice (Q64.64), active liquidity, dynamic feeRate (ppm) and vaults.
 */
export const WHIRLPOOL_ACCOUNT_SIZE = 653;
export const WHIRLPOOL_PROGRAM_ID = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

export interface WhirlpoolDecoded {
  tickSpacing: number;
  feeRatePpm: bigint;
  protocolFeeRate: number;
  liquidity: bigint;
  sqrtPriceX64: bigint;
  tickCurrentIndex: number;
  tokenMintA: string;
  tokenVaultA: string;
  tokenMintB: string;
  tokenVaultB: string;
}

export function decodeWhirlpool(data: Buffer): WhirlpoolDecoded | null {
  if (data.length !== WHIRLPOOL_ACCOUNT_SIZE) return null;
  const r = new BufferReader(data);

  r.skip(8); // Anchor discriminator
  r.skip(32); // whirlpoolsConfig
  r.skip(1); // whirlpoolBump
  const tickSpacing = r.u16();
  r.skip(2); // tickSpacingSeed
  const feeRatePpm = BigInt(r.u16()); // hundredths of a bp => ppm of input
  const protocolFeeRate = r.u16();
  const liquidity = r.u128();
  const sqrtPriceX64 = r.u128();
  const tickCurrentIndex = r.i32();
  r.skip(8 + 8); // protocolFeeOwedA, protocolFeeOwedB
  const tokenMintA = r.pubkey();
  const tokenVaultA = r.pubkey();
  r.skip(16); // feeGrowthGlobalA
  const tokenMintB = r.pubkey();
  const tokenVaultB = r.pubkey();
  // remaining: feeGrowthGlobalB, rewardLastUpdatedTimestamp, rewardInfos[3]

  return {
    tickSpacing,
    feeRatePpm,
    protocolFeeRate,
    liquidity,
    sqrtPriceX64,
    tickCurrentIndex,
    tokenMintA,
    tokenVaultA,
    tokenMintB,
    tokenVaultB,
  };
}
