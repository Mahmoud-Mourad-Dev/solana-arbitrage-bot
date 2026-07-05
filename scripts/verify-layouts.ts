/**
 * Layout verification against live mainnet accounts (read-only).
 * Decodes the flagship Raydium v4 and Orca Whirlpool SOL/USDC pools and
 * cross-checks that both venues imply the same SOL price — if any parser
 * offset were wrong, mints/decimals/prices would disagree loudly.
 *
 * Run: npm run verify:layouts
 */
import { Connection, PublicKey } from "@solana/web3.js";
import { decodeRaydiumV4 } from "../src/parsers/raydium";
import { decodeWhirlpool } from "../src/parsers/whirlpool";
import { decodeTokenAccountAmount } from "../src/parsers/spl";
import { WSOL_MINT, USDC_MINT } from "../src/config";

async function main(): Promise<void> {
  const conn = new Connection(
    process.env.RPC_ENDPOINT || "https://api.mainnet-beta.solana.com",
    "confirmed",
  );

  const rayAddr = new PublicKey("58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2");
  const orcaAddr = new PublicKey("HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ");
  const [ray, orca] = await conn.getMultipleAccountsInfo([rayAddr, orcaAddr]);

  if (!ray) throw new Error("raydium pool not found");
  const r = decodeRaydiumV4(Buffer.from(ray.data));
  if (!r) throw new Error(`raydium decode failed (len=${ray.data.length})`);
  console.log("RAYDIUM SOL/USDC:", {
    status: r.status.toString(),
    baseDecimal: r.baseDecimal,
    quoteDecimal: r.quoteDecimal,
    swapFee: `${r.swapFeeNumerator}/${r.swapFeeDenominator}`,
    baseMint: r.baseMint,
    quoteMint: r.quoteMint,
    baseNeedTakePnl: r.baseNeedTakePnl.toString(),
    quoteNeedTakePnl: r.quoteNeedTakePnl.toString(),
    poolOpenTime: r.poolOpenTime.toString(),
  });
  if (r.baseMint !== WSOL_MINT || r.quoteMint !== USDC_MINT) {
    throw new Error("MINT MISMATCH — Raydium layout wrong");
  }
  if (r.baseDecimal !== 9 || r.quoteDecimal !== 6) throw new Error("DECIMALS MISMATCH");

  const [va, vb] = await conn.getMultipleAccountsInfo([
    new PublicKey(r.baseVault),
    new PublicKey(r.quoteVault),
  ]);
  const aBal = decodeTokenAccountAmount(Buffer.from(va!.data))!;
  const bBal = decodeTokenAccountAmount(Buffer.from(vb!.data))!;
  const solPrice = Number(bBal) / 1e6 / (Number(aBal) / 1e9);
  console.log(
    `  vaults: ${(Number(aBal) / 1e9).toFixed(2)} SOL / ${(Number(bBal) / 1e6).toFixed(2)} USDC` +
      ` -> implied SOL price ~$${solPrice.toFixed(2)}`,
  );

  if (!orca) throw new Error("whirlpool not found");
  const w = decodeWhirlpool(Buffer.from(orca.data));
  if (!w) throw new Error(`whirlpool decode failed (len=${orca.data.length})`);
  const sqrt = Number(w.sqrtPriceX64) / 2 ** 64;
  const price = sqrt * sqrt * 1e3; // USDC-raw/lamport -> USDC/SOL (decimals 9->6)
  console.log("ORCA WHIRLPOOL SOL/USDC:", {
    tickSpacing: w.tickSpacing,
    feeRatePpm: w.feeRatePpm.toString(),
    tickCurrentIndex: w.tickCurrentIndex,
    liquidity: w.liquidity.toString(),
    mintA: w.tokenMintA,
    mintB: w.tokenMintB,
  });
  console.log(`  implied SOL price from sqrtPriceX64 ~$${price.toFixed(2)}`);
  if (w.tokenMintA !== WSOL_MINT || w.tokenMintB !== USDC_MINT) {
    throw new Error("MINT MISMATCH — Whirlpool layout wrong");
  }
  const tickPrice = Math.pow(1.0001, w.tickCurrentIndex) * 1e3;
  console.log(`  cross-check via tickCurrentIndex ~$${tickPrice.toFixed(2)}`);
  if (Math.abs(tickPrice - price) / price > 0.01) {
    throw new Error("tick/sqrtPrice disagree — Whirlpool layout wrong");
  }
  if (Math.abs(solPrice - price) / price > 0.02) {
    throw new Error("Raydium vs Orca implied price disagree >2%");
  }
  console.log("LAYOUT VERIFICATION OK — both venues agree on live SOL price");
}

main().catch((e) => {
  console.error("FAILED:", e instanceof Error ? e.message : e);
  process.exit(1);
});
