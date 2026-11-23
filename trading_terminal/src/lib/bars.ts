// OHLCV bars for the terminal chart.
//
// The market-data service publishes `kline_*` frames, but the engine only
// emits a bar for a window when a *later* fill lands in the same (or a newer)
// window: the first fill of every window is suppressed and nothing ticks on a
// timer. A quiet market therefore produces no kline frames at all, and the
// last bar never closes.
//
// So the terminal derives bars from the trade stream, which is always flowing,
// and layers the service's bars on top: a trade gives an instant candle, a
// server bar is authoritative for the window it covers (it carries the exact
// volume/trade count and the closed flag). Both use the same window anchor —
// `floor(ts / interval) * interval` — so they merge cleanly.

/** Intervals the terminal chart offers, matching the advertised kline streams. */
export const BAR_INTERVALS_MS = { "1s": 1_000, "1m": 60_000 } as const;

export type BarInterval = keyof typeof BAR_INTERVALS_MS;

export interface Bar {
  /** Window start in seconds, the value lightweight-charts takes as `time`. */
  time: number;
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  /** Set by a server bar that has rolled over; trades never clear it. */
  closed: boolean;
}

/** Newest windows kept per interval; older ones fall off the left edge. */
const MAX_WINDOWS = 1_800;

function isInterval(label: string): label is BarInterval {
  return Object.hasOwn(BAR_INTERVALS_MS, label);
}

export class BarStore {
  private windows = new Map<BarInterval, Map<number, Bar>>();

  reset(): void {
    this.windows.clear();
  }

  /** Folds one trade print into every interval. */
  addTrade(tsMs: number, price: number, volume: number): void {
    if (!Number.isFinite(tsMs) || tsMs <= 0 || !Number.isFinite(price) || price <= 0) return;
    const size = Number.isFinite(volume) && volume > 0 ? volume : 0;

    for (const label of Object.keys(BAR_INTERVALS_MS) as BarInterval[]) {
      const intervalMs = BAR_INTERVALS_MS[label];
      const time = Math.floor(tsMs / intervalMs) * (intervalMs / 1_000);
      const book = this.book(label);
      const bar = book.get(time);
      if (bar) {
        bar.high = Math.max(bar.high, price);
        bar.low = Math.min(bar.low, price);
        bar.close = price;
        bar.volume += size;
      } else {
        book.set(time, { time, open: price, high: price, low: price, close: price, volume: size, closed: false });
        this.trim(book);
      }
    }
  }

  /**
   * Replaces a window with a server bar. A closed bar is final: later partial
   * frames for the same window (which the service still sends from its cache)
   * cannot reopen or rewrite it.
   */
  applyServerBar(
    interval: string,
    bar: { time: number; open: number; high: number; low: number; close: number; volume: number; closed: boolean },
  ): void {
    if (!isInterval(interval)) return;
    const numbers = [bar.time, bar.open, bar.high, bar.low, bar.close, bar.volume];
    if (!numbers.every((value) => Number.isFinite(value)) || bar.time <= 0) return;

    const book = this.book(interval);
    if (book.get(bar.time)?.closed) return;
    book.set(bar.time, { ...bar });
    this.trim(book);
  }

  /**
   * Seeds the store with bars read from `GET /klines`.
   *
   * Values arrive as canonical decimal strings on the wire and are narrowed to
   * chart numbers here; the store and API keep all 18 digits. A closed history
   * bar is final and is never reopened by a later partial frame.
   */
  applyHistoryBars(interval: string, bars: Bar[]): void {
    if (!isInterval(interval) || !bars.length) return;
    const book = this.book(interval);
    for (const bar of bars) {
      if (!Number.isFinite(bar.time) || bar.time <= 0) continue;
      const existing = book.get(bar.time);
      if (existing?.closed && !bar.closed) continue;
      book.set(bar.time, { ...bar });
    }
    this.trim(book);
  }

  /** Ascending-time copy for the chart. */
  series(interval: BarInterval): Bar[] {
    const book = this.windows.get(interval);
    if (!book) return [];
    return [...book.values()].sort((left, right) => left.time - right.time);
  }

  private book(interval: BarInterval): Map<number, Bar> {
    let book = this.windows.get(interval);
    if (!book) {
      book = new Map();
      this.windows.set(interval, book);
    }
    return book;
  }

  private trim(book: Map<number, Bar>): void {
    if (book.size <= MAX_WINDOWS) return;
    const oldest = [...book.keys()].sort((left, right) => left - right);
    for (const time of oldest.slice(0, book.size - MAX_WINDOWS)) book.delete(time);
  }
}
