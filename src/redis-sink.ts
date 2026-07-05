import Redis from "ioredis";
import type { ArbitrageCycle, PoolState } from "./types";
import type { MonitorConfig } from "./config";
import { raydiumEffectiveReserves } from "./math";
import { jsonStringifyBigint, Logger } from "./utils";

/**
 * Batched Redis writer.
 *
 * Pool-state HSETs are buffered into an ioredis pipeline and flushed either
 * every `redisFlushIntervalMs` or once `redisFlushMaxCommands` commands have
 * accumulated — one RTT amortized over the whole batch keeps per-update
 * overhead well under 1ms. Opportunities bypass the buffer: they are
 * latency-critical and go out immediately as LPUSH+LTRIM+PUBLISH in a
 * single pipeline.
 */
export class RedisSink {
  private readonly redis: Redis;
  private pipeline: ReturnType<Redis["pipeline"]>;
  private pending = 0;
  private flushTimer: NodeJS.Timeout | null = null;
  private closed = false;

  constructor(
    private readonly config: MonitorConfig,
    private readonly log: Logger,
  ) {
    this.redis = new Redis(config.redisUrl, {
      maxRetriesPerRequest: null,
      enableOfflineQueue: true,
      lazyConnect: false,
    });
    this.redis.on("error", (err) => this.log.warn(`redis error: ${err.message}`));
    this.pipeline = this.redis.pipeline();
  }

  /** Buffer the latest quotable state of a pool (hot path — no awaits). */
  queuePoolState(pool: PoolState): void {
    if (this.closed) return;
    const key = `pool:${pool.address}`;
    const common = {
      dex: pool.dex,
      mintA: pool.mintA,
      mintB: pool.mintB,
      slot: pool.lastSlot.toString(),
      updatedAtMs: pool.lastUpdatedMs.toString(),
    };
    if (pool.dex === "raydium-v4") {
      const reserves = raydiumEffectiveReserves(pool);
      this.pipeline.hset(key, {
        ...common,
        reserveBase: reserves ? reserves.base.toString() : "0",
        reserveQuote: reserves ? reserves.quote.toString() : "0",
        feeNumerator: pool.swapFeeNumerator.toString(),
        feeDenominator: pool.swapFeeDenominator.toString(),
        status: pool.status.toString(),
      });
    } else {
      this.pipeline.hset(key, {
        ...common,
        sqrtPriceX64: pool.sqrtPriceX64.toString(),
        liquidity: pool.liquidity.toString(),
        tickCurrentIndex: pool.tickCurrentIndex.toString(),
        feeRatePpm: pool.feeRatePpm.toString(),
      });
    }
    this.pending++;
    if (this.pending >= this.config.redisFlushMaxCommands) {
      this.flush();
    } else if (!this.flushTimer) {
      this.flushTimer = setTimeout(() => this.flush(), this.config.redisFlushIntervalMs);
    }
  }

  private flush(): void {
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = null;
    }
    if (this.pending === 0) return;
    const batch = this.pipeline;
    this.pipeline = this.redis.pipeline();
    this.pending = 0;
    batch.exec().catch((err: Error) => this.log.warn(`redis flush failed: ${err.message}`));
  }

  /** Immediate fan-out of a profitable cycle — LPUSH + LTRIM + PUBLISH. */
  publishOpportunity(cycle: ArbitrageCycle): void {
    if (this.closed) return;
    const payload = jsonStringifyBigint(cycle);
    const p = this.redis.pipeline();
    p.lpush(this.config.opportunityList, payload);
    p.ltrim(this.config.opportunityList, 0, this.config.opportunityListMax - 1);
    p.publish(this.config.opportunityChannel, payload);
    p.exec().catch((err: Error) => this.log.warn(`opportunity publish failed: ${err.message}`));
  }

  async close(): Promise<void> {
    this.closed = true;
    this.flush();
    await this.redis.quit().catch(() => this.redis.disconnect());
  }
}
