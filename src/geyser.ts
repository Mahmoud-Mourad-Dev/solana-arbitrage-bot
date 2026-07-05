import Client, { CommitmentLevel, SubscribeRequest } from "@triton-one/yellowstone-grpc";
import type { ClientDuplexStream } from "@grpc/grpc-js";
import { Logger } from "./utils";

export interface RawAccountUpdate {
  pubkey: Buffer;
  data: Buffer;
  slot: bigint;
}

const PING_INTERVAL_MS = 10_000;
const BACKOFF_INITIAL_MS = 500;
const BACKOFF_MAX_MS = 30_000;

function emptySubscribeRequest(): SubscribeRequest {
  return {
    accounts: {},
    slots: {},
    transactions: {},
    transactionsStatus: {},
    blocks: {},
    blocksMeta: {},
    entry: {},
    accountsDataSlice: [],
  } as SubscribeRequest;
}

/**
 * Persistent Yellowstone Geyser subscription over one bidirectional gRPC
 * stream, at PROCESSED commitment for minimum latency.
 *
 * - Reconnects with exponential backoff + jitter; backoff resets after the
 *   first packet arrives on the new stream.
 * - Application-level pings every 10s keep intermediaries from idling out
 *   the connection.
 * - `setAccounts` can be called at any time (e.g. after bootstrap discovers
 *   vault addresses) — the filter is rewritten onto the live stream without
 *   dropping it.
 */
export class GeyserStreamManager {
  private client: Client;
  private stream: ClientDuplexStream<SubscribeRequest, any> | null = null;
  private accounts: string[] = [];
  private backoffMs = BACKOFF_INITIAL_MS;
  private pingTimer: NodeJS.Timeout | null = null;
  private reconnectTimer: NodeJS.Timeout | null = null;
  private pingId = 0;
  private stopping = false;
  private sawDataOnStream = false;

  constructor(
    endpoint: string,
    xToken: string | undefined,
    private readonly log: Logger,
    private readonly onAccount: (update: RawAccountUpdate) => void,
  ) {
    this.client = new Client(endpoint, xToken, {
      "grpc.max_receive_message_length": 64 * 1024 * 1024,
      "grpc.keepalive_time_ms": 20_000,
      "grpc.keepalive_timeout_ms": 5_000,
      "grpc.keepalive_permit_without_calls": 1,
    });
  }

  async start(accounts: string[]): Promise<void> {
    this.accounts = accounts;
    await this.connect();
  }

  /** Replace the watched account set on the live stream. */
  async setAccounts(accounts: string[]): Promise<void> {
    this.accounts = accounts;
    if (this.stream) {
      await this.writeRequest(this.buildRequest()).catch((err: Error) => {
        this.log.warn(`filter update failed, forcing reconnect: ${err.message}`);
        this.stream?.destroy(err);
      });
    }
  }

  private buildRequest(): SubscribeRequest {
    const req = emptySubscribeRequest();
    req.accounts = {
      pools: { account: this.accounts, owner: [], filters: [] },
    };
    req.commitment = CommitmentLevel.PROCESSED;
    return req;
  }

  private async connect(): Promise<void> {
    if (this.stopping) return;
    this.sawDataOnStream = false;
    try {
      const stream = await this.client.subscribe();
      this.stream = stream;

      stream.on("data", (data: any) => {
        if (!this.sawDataOnStream) {
          this.sawDataOnStream = true;
          this.backoffMs = BACKOFF_INITIAL_MS;
        }
        if (data?.account?.account) {
          const acc = data.account.account;
          this.onAccount({
            pubkey: Buffer.from(acc.pubkey),
            data: Buffer.from(acc.data),
            slot: BigInt(data.account.slot),
          });
        }
        // data.pong needs no handling — receipt itself proves liveness.
      });
      stream.on("error", (err: Error) => {
        this.log.warn(`geyser stream error: ${err.message}`);
        this.teardownStream();
        this.scheduleReconnect();
      });
      stream.on("end", () => {
        this.log.warn("geyser stream ended by server");
        this.teardownStream();
        this.scheduleReconnect();
      });
      stream.on("close", () => {
        this.teardownStream();
        this.scheduleReconnect();
      });

      await this.writeRequest(this.buildRequest());
      this.startPing();
      this.log.info(`geyser subscribed: ${this.accounts.length} accounts @ processed commitment`);
    } catch (err) {
      this.log.warn(`geyser connect failed: ${(err as Error).message}`);
      this.teardownStream();
      this.scheduleReconnect();
    }
  }

  private writeRequest(req: SubscribeRequest): Promise<void> {
    return new Promise((resolve, reject) => {
      const stream = this.stream;
      if (!stream) return reject(new Error("stream not connected"));
      stream.write(req, (err: Error | null | undefined) => (err ? reject(err) : resolve()));
    });
  }

  private startPing(): void {
    this.stopPing();
    this.pingTimer = setInterval(() => {
      const req = emptySubscribeRequest();
      req.ping = { id: ++this.pingId };
      this.writeRequest(req).catch(() => {
        // Write failure will surface via the stream 'error' handler.
      });
    }, PING_INTERVAL_MS);
  }

  private stopPing(): void {
    if (this.pingTimer) {
      clearInterval(this.pingTimer);
      this.pingTimer = null;
    }
  }

  private teardownStream(): void {
    this.stopPing();
    if (this.stream) {
      this.stream.removeAllListeners();
      try {
        this.stream.destroy();
      } catch {
        /* already destroyed */
      }
      this.stream = null;
    }
  }

  private scheduleReconnect(): void {
    if (this.stopping || this.reconnectTimer) return;
    const jitter = Math.floor(Math.random() * this.backoffMs * 0.3);
    const delay = this.backoffMs + jitter;
    this.log.info(`geyser reconnecting in ${delay}ms`);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      void this.connect();
    }, delay);
    this.backoffMs = Math.min(this.backoffMs * 2, BACKOFF_MAX_MS);
  }

  stop(): void {
    this.stopping = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.teardownStream();
  }
}
