// Depth assembler, ported from `market_data_system/web/app.js`
// (`claimBook`/`applySnapshot`/`applyUpdate`/`applyLevels`).
//
// The Binance contract: a subscribe-time `depthSnapshot` carries
// `lastUpdateId`; every following `depthUpdate` must satisfy
// `U <= lastUpdateId + 1 <= u`. A gap means the book is stale until a fresh
// snapshot arrives (the server also pushes `resync` in that case).

import type { DepthLevel, DepthSnapshotFrame, DepthUpdateFrame, SnapshotLevel } from "./types";

export interface BookView {
  bids: [string, string][];
  asks: [string, string][];
  lastUpdateId: number | null;
  stale: boolean;
  source: "snapshot" | "partial" | "diff" | null;
}

export interface Bbo {
  bid: string | null;
  bidSize: string | null;
  ask: string | null;
  askSize: string | null;
}

export interface ApplyOutcome {
  applied: boolean;
  stale: boolean;
  note: string;
}

const EMPTY_VIEW: BookView = {
  bids: [],
  asks: [],
  lastUpdateId: null,
  stale: false,
  source: null,
};

export class DepthBook {
  private bids = new Map<string, string>();
  private asks = new Map<string, string>();
  private lastUpdateId: number | null = null;
  private stale = false;
  private source: BookView["source"] = null;

  reset(): void {
    this.bids.clear();
    this.asks.clear();
    this.lastUpdateId = null;
    this.stale = false;
    this.source = null;
  }

  markStale(reason: string): ApplyOutcome {
    this.stale = true;
    return { applied: false, stale: true, note: `Resync requested (${reason}); waiting for a fresh depth snapshot.` };
  }

  applySnapshot(frame: DepthSnapshotFrame): ApplyOutcome {
    this.bids.clear();
    this.asks.clear();
    for (const [price, size] of frame.bids ?? []) this.bids.set(price, size);
    for (const [price, size] of frame.asks ?? []) this.asks.set(price, size);
    this.lastUpdateId = frame.lastUpdateId ?? null;
    this.stale = false;
    this.source = "snapshot";
    return {
      applied: true,
      stale: false,
      note: `lastUpdateId ${this.lastUpdateId ?? "—"}; updates must be contiguous from here.`,
    };
  }

  applyUpdate(frame: DepthUpdateFrame): ApplyOutcome {
    if (this.stale) {
      return { applied: false, stale: true, note: "Book is stale; waiting for a snapshot." };
    }
    if (this.lastUpdateId === null) {
      return { applied: false, stale: false, note: "Waiting for a depth snapshot before applying updates." };
    }
    if (Number(frame.u) <= this.lastUpdateId) {
      return { applied: false, stale: false, note: "" };
    }
    const first = Number(frame.U);
    const last = Number(frame.u);
    if (first <= this.lastUpdateId + 1 && last >= this.lastUpdateId + 1) {
      applyLevels(frame.b ?? [], this.bids);
      applyLevels(frame.a ?? [], this.asks);
      this.lastUpdateId = last;
      this.source = "diff";
      return {
        applied: true,
        stale: false,
        note: `lastUpdateId ${last} · ${(frame.b ?? []).length + (frame.a ?? []).length} level change(s)`,
      };
    }
    this.stale = true;
    return {
      applied: false,
      stale: true,
      note: `Gap: expected U <= ${this.lastUpdateId + 1}, got ${first}. Waiting for a resync snapshot.`,
    };
  }

  /** Latest-value bookTicker frames keep the BBO fresh when depth is quiet. */
  applyBbo(bid: string | null, bidSize: string | null, ask: string | null, askSize: string | null): Bbo {
    return { bid, bidSize, ask, askSize };
  }

  view(depthRows: number): BookView {
    if (this.source === null && this.lastUpdateId === null && !this.bids.size && !this.asks.size) {
      return { ...EMPTY_VIEW };
    }
    const bids = [...this.bids.entries()]
      .sort((a, b) => Number(b[0]) - Number(a[0]))
      .slice(0, depthRows);
    const asks = [...this.asks.entries()]
      .sort((a, b) => Number(a[0]) - Number(b[0]))
      .slice(0, depthRows);
    return {
      bids,
      asks,
      lastUpdateId: this.lastUpdateId,
      stale: this.stale,
      source: this.source,
    };
  }

  /** Best bid/ask derived from the assembled ladder (spread row + ticket). */
  best(): Bbo {
    const bids = [...this.bids.entries()].sort((a, b) => Number(b[0]) - Number(a[0]));
    const asks = [...this.asks.entries()].sort((a, b) => Number(a[0]) - Number(b[0]));
    const bid = bids[0] ?? null;
    const ask = asks[0] ?? null;
    return {
      bid: bid ? bid[0] : null,
      bidSize: bid ? bid[1] : null,
      ask: ask ? ask[0] : null,
      askSize: ask ? ask[1] : null,
    };
  }
}

function applyLevels(levels: DepthLevel[] | SnapshotLevel[], map: Map<string, string>): void {
  for (const entry of levels) {
    const [price, size] = entry;
    if (Number(size) === 0) map.delete(price);
    else map.set(price, size);
  }
}

/** Keep the last complete top of book: an empty side is not a new quote. */
export function mergeBbo(previous: Bbo, frame: { b: string; B: string; a: string; A: string }): Bbo {
  const bid = Number(frame.b) > 0 ? frame.b : previous.bid;
  const ask = Number(frame.a) > 0 ? frame.a : previous.ask;
  const bidSize = frame.B !== undefined && frame.B !== null && frame.B !== "" ? frame.B : previous.bidSize;
  const askSize = frame.A !== undefined && frame.A !== null && frame.A !== "" ? frame.A : previous.askSize;
  return { bid, bidSize, ask, askSize };
}
