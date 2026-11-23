// Market-data WebSocket client, ported from `market_data_system/web/app.js`.
//
// One socket to the global terminal service (`/md/ws` behind the nginx proxy),
// Binance-style control frames, live symbol switches by re-subscribing the
// selected symbol's streams on the same connection. Frames carry `s` (symbol)
// on every payload, so late frames from the previous symbol are dropped.

export const DEPTH_SPEEDS = ["100ms", "1000ms"] as const;

/** Streams a terminal needs, in preference order. */
export const TERMINAL_STREAMS = [
  "bookTicker",
  "depth@100ms",
  "trade",
  "kline_1s",
  "kline_1m",
  "status",
] as const;

export interface StreamKey {
  symbol: string;
  stream: string;
}

export function streamName(symbol: string, stream: string): string {
  return `${symbol}@${stream}`;
}

export function parseStreamKey(name: string): StreamKey | null {
  const at = name.indexOf("@");
  if (at <= 0) return null;
  return { symbol: name.slice(0, at), stream: name.slice(at + 1) };
}

export type FeedStatus = "connecting" | "connected" | "closed" | "error";

export interface FeedEnvelope {
  /** Combined-stream wrapper (`/stream`), absent on raw `/ws` frames. */
  stream?: string;
  data: Record<string, unknown>;
}

export type FrameHandler = (envelope: FeedEnvelope) => void;
export type StatusHandler = (status: FeedStatus, detail?: string) => void;

let controlSeq = 0;

export class MarketFeed {
  private ws: WebSocket | null = null;
  private url: string;
  private subscribed = new Set<string>();
  private wanted = new Set<string>();
  private frameHandlers = new Set<FrameHandler>();
  private statusHandlers = new Set<StatusHandler>();
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private reconnectDelay = 1000;
  private closedByUser = false;

  constructor(url = "/md/ws") {
    this.url = url;
  }

  onFrame(handler: FrameHandler): () => void {
    this.frameHandlers.add(handler);
    return () => this.frameHandlers.delete(handler);
  }

  onStatus(handler: StatusHandler): () => void {
    this.statusHandlers.add(handler);
    return () => this.statusHandlers.delete(handler);
  }

  connect(): void {
    this.closedByUser = false;
    if (this.ws && (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING)) {
      return;
    }
    this.emitStatus("connecting");
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.url);
    } catch (error) {
      this.emitStatus("error", String(error));
      this.scheduleReconnect();
      return;
    }
    this.ws = ws;
    ws.addEventListener("open", () => {
      this.reconnectDelay = 1000;
      this.emitStatus("connected");
      this.subscribed.clear();
      this.sync();
    });
    ws.addEventListener("message", (event) => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(String(event.data));
      } catch {
        return;
      }
      const envelope = this.unwrap(parsed);
      if (envelope) {
        for (const handler of this.frameHandlers) handler(envelope);
      }
    });
    ws.addEventListener("close", (event) => {
      const wasConnected = this.ws === ws;
      this.ws = null;
      this.subscribed.clear();
      if (wasConnected) this.emitStatus("closed", event.reason || `closed (${event.code})`);
      if (!this.closedByUser) this.scheduleReconnect();
    });
    ws.addEventListener("error", () => {
      this.emitStatus("error", "WebSocket error");
    });
  }

  disconnect(): void {
    this.closedByUser = true;
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    const ws = this.ws;
    this.ws = null;
    this.subscribed.clear();
    if (ws) {
      try {
        ws.close(1000, "client disconnect");
      } catch {
        // already closing
      }
    }
    this.emitStatus("closed");
  }

  /** Set the exact set of stream names the socket should carry. */
  setWanted(names: Iterable<string>): void {
    this.wanted = new Set(names);
    this.sync();
  }

  private sync(): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    const add: string[] = [];
    const remove: string[] = [];
    for (const name of this.wanted) {
      if (!this.subscribed.has(name)) add.push(name);
    }
    for (const name of this.subscribed) {
      if (!this.wanted.has(name)) remove.push(name);
    }
    if (add.length) this.send("SUBSCRIBE", add);
    if (remove.length) this.send("UNSUBSCRIBE", remove);
    for (const name of add) this.subscribed.add(name);
    for (const name of remove) this.subscribed.delete(name);
  }

  private send(method: string, params: string[]): void {
    const ws = this.ws;
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    controlSeq += 1;
    try {
      ws.send(JSON.stringify({ method, params, id: controlSeq }));
    } catch {
      // the close handler schedules the reconnect
    }
  }

  private unwrap(parsed: unknown): FeedEnvelope | null {
    if (!parsed || typeof parsed !== "object") return null;
    const raw = parsed as Record<string, unknown>;
    // Combined-stream envelope from `/stream?streams=...`
    if (typeof raw.stream === "string" && raw.data && typeof raw.data === "object") {
      return { stream: raw.stream, data: raw.data as Record<string, unknown> };
    }
    // Control replies.
    if ("result" in raw || ("code" in raw && "msg" in raw) || "error" in raw) {
      return { data: raw };
    }
    // Raw `/ws` payload; attribute it to the advertised stream by event type.
    return { data: raw };
  }

  private emitStatus(status: FeedStatus, detail?: string): void {
    for (const handler of this.statusHandlers) handler(status, detail);
  }

  private scheduleReconnect(): void {
    if (this.closedByUser || this.reconnectTimer) return;
    const delay = this.reconnectDelay;
    this.reconnectDelay = Math.min(this.reconnectDelay * 2, 5000);
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }
}
