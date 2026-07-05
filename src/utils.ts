import bs58 from "bs58";
import { createHash } from "crypto";

/** Zero-copy little-endian cursor over a raw account buffer. */
export class BufferReader {
  private offset = 0;
  constructor(private readonly buf: Buffer) {}

  get remaining(): number {
    return this.buf.length - this.offset;
  }

  skip(n: number): this {
    this.offset += n;
    return this;
  }

  u8(): number {
    const v = this.buf.readUInt8(this.offset);
    this.offset += 1;
    return v;
  }

  u16(): number {
    const v = this.buf.readUInt16LE(this.offset);
    this.offset += 2;
    return v;
  }

  i32(): number {
    const v = this.buf.readInt32LE(this.offset);
    this.offset += 4;
    return v;
  }

  u64(): bigint {
    const v = this.buf.readBigUInt64LE(this.offset);
    this.offset += 8;
    return v;
  }

  u128(): bigint {
    const lo = this.buf.readBigUInt64LE(this.offset);
    const hi = this.buf.readBigUInt64LE(this.offset + 8);
    this.offset += 16;
    return (hi << 64n) | lo;
  }

  pubkey(): string {
    const v = bs58.encode(this.buf.subarray(this.offset, this.offset + 32));
    this.offset += 32;
    return v;
  }
}

export function shortHash(input: string): string {
  return createHash("sha256").update(input).digest("hex").slice(0, 16);
}

/** JSON.stringify that serializes BigInt as decimal strings. */
export function jsonStringifyBigint(value: unknown): string {
  return JSON.stringify(value, (_k, v) => (typeof v === "bigint" ? v.toString() : v));
}

/** Human formatting for logs only — never used in trading math. */
export function formatUnits(amount: bigint, decimals: number): string {
  const neg = amount < 0n;
  const abs = neg ? -amount : amount;
  const base = 10n ** BigInt(decimals);
  const whole = abs / base;
  const frac = (abs % base).toString().padStart(decimals, "0").slice(0, 6);
  return `${neg ? "-" : ""}${whole}.${frac}`;
}

export function ceilDiv(a: bigint, b: bigint): bigint {
  return (a + b - 1n) / b;
}

type LogLevel = "debug" | "info" | "warn" | "error";
const LEVEL_ORDER: Record<LogLevel, number> = { debug: 0, info: 1, warn: 2, error: 3 };

export class Logger {
  constructor(private readonly minLevel: LogLevel) {}

  private log(level: LogLevel, msg: string, extra?: unknown): void {
    if (LEVEL_ORDER[level] < LEVEL_ORDER[this.minLevel]) return;
    const line = `${new Date().toISOString()} [${level.toUpperCase()}] ${msg}`;
    if (extra !== undefined) {
      // eslint-disable-next-line no-console
      console[level === "debug" ? "log" : level](line, extra);
    } else {
      // eslint-disable-next-line no-console
      console[level === "debug" ? "log" : level](line);
    }
  }

  debug(msg: string, extra?: unknown): void {
    this.log("debug", msg, extra);
  }
  info(msg: string, extra?: unknown): void {
    this.log("info", msg, extra);
  }
  warn(msg: string, extra?: unknown): void {
    this.log("warn", msg, extra);
  }
  error(msg: string, extra?: unknown): void {
    this.log("error", msg, extra);
  }
}
