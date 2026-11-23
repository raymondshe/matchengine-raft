// Market-feed provider: one WebSocket to the global terminal service, one
// active symbol at a time (Binance-style). Ports the market-data test page's
// frame handling into a throttled React state machine: depth/trades/bbo
// accumulate in refs and flush at ~10 fps so the ladder stays smooth without
// re-rendering per frame.

import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { BarStore, type Bar, type BarInterval } from "../lib/bars";
import { DepthBook, mergeBbo, type Bbo, type BookView } from "../lib/book";
import { MarketFeed, TERMINAL_STREAMS, streamName, type FeedStatus } from "../lib/feed";
import type { MdInfo, MdReady, StatusFrame, TradeFrame } from "../lib/types";

export type KlineInterval = BarInterval;

export interface TapeEntry {
  id: number;
  time: string;
  title: string;
  side: "BUY" | "SELL";
  price: string;
  qty: string;
}

export interface KlineTick {
  time: number; // seconds
  open: number;
  high: number;
  low: number;
  close: number;
  volume: number;
  interval: string;
  closed: boolean;
}

interface MarketFeedState {
  symbols: MdInfo["symbols"];
  advertised: string[];
  fastOnly: boolean;
  symbol: string | null;
  setSymbol: (symbol: string) => void;
  interval: KlineInterval;
  setInterval_: (interval: KlineInterval) => void;
  status: FeedStatus;
  connDetail: string;
  frameCount: number;
  bbo: Bbo;
  book: BookView;
  bookNote: string;
  trades: TapeEntry[];
  statusFrame: StatusFrame | null;
  subscribeBars: (interval: BarInterval, handler: (bars: Bar[]) => void) => () => void;
  ready: MdReady | null;
  /** Bars seeded from TDengine by `GET /md/klines` for the active pair/interval. */
  historyCount: number;
  historyError: string | null;
}

const MarketFeedContext = createContext<MarketFeedState | null>(null);

const MAX_TRADES = 60;
const DEPTH_ROWS = 12;
const FLUSH_MS = 100;

export function MarketFeedProvider({ children }: { children: ReactNode }) {
  const feedRef = useRef<MarketFeed | null>(null);
  const [info, setInfo] = useState<MdInfo | null>(null);
  const [symbol, setSymbolState] = useState<string | null>(null);
  const [interval_, setIntervalState] = useState<KlineInterval>("1s");
  const [status, setStatus] = useState<FeedStatus>("closed");
  const [connDetail, setConnDetail] = useState<string>("");
  const [frameCount, setFrameCount] = useState(0);
  const [ready, setReady] = useState<MdReady | null>(null);
  const [historyCount, setHistoryCount] = useState(0);
  const [historyError, setHistoryError] = useState<string | null>(null);

  const bookRef = useRef(new DepthBook());
  const bboRef = useRef<Bbo>({ bid: null, bidSize: null, ask: null, askSize: null });
  const tradesRef = useRef<TapeEntry[]>([]);
  const tradeSeq = useRef(0);
  const statusRef = useRef<StatusFrame | null>(null);
  const noteRef = useRef("Waiting for depth.");
  const symbolRef = useRef<string | null>(null);
  const intervalRef = useRef<KlineInterval>("1s");
  const frameCounter = useRef(0);
  // Bars come from the trade stream (always flowing) with server kline frames
  // layered on top; see `lib/bars.ts` for why the tape is the primary source.
  const barsRef = useRef(new BarStore());
  const barHandlers = useRef(new Set<{ interval: BarInterval; handler: (bars: Bar[]) => void }>());

  const [bbo, setBbo] = useState<Bbo>(bboRef.current);
  const [book, setBook] = useState<BookView>(() => bookRef.current.view(DEPTH_ROWS));
  const [bookNote, setBookNote] = useState(noteRef.current);
  const [trades, setTrades] = useState<TapeEntry[]>([]);
  const [statusFrame, setStatusFrame] = useState<StatusFrame | null>(null);

  // ---- load /info once, then keep /ready fresh ----
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const response = await fetch("/md/info");
        const parsed = (await response.json()) as MdInfo;
        if (!cancelled) {
          setInfo(parsed);
          if (parsed.symbols.length) setSymbolState(parsed.symbols[0].symbol);
        }
      } catch {
        if (!cancelled) setConnDetail("could not load /md/info");
      }
    })();
    const timer = setInterval(() => {
      fetch("/md/ready")
        .then((response) => (response.ok ? response.json() : null))
        .then((parsed: MdReady | null) => {
          if (!cancelled) setReady(parsed);
        })
        .catch(() => undefined);
    }, 2000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, []);

  // ---- one feed instance for the lifetime of the app ----
  useEffect(() => {
    const feed = new MarketFeed("/md/ws");
    feedRef.current = feed;

    const offStatus = feed.onStatus((next, detail) => {
      setStatus(next);
      setConnDetail(detail ?? "");
    });

    const offFrames = feed.onFrame(({ data }) => {
      const active = symbolRef.current;
      const kind = String(data.e ?? "");
      const frameSymbol = typeof data.s === "string" ? data.s : null;
      if (frameSymbol && active && frameSymbol !== active) return;

      frameCounter.current += 1;

      if (kind === "bookTicker") {
        const b = String(data.b ?? "");
        const a = String(data.a ?? "");
        if (Number(b) > 0 && Number(a) > 0) {
          bboRef.current = mergeBbo(bboRef.current, {
            b,
            B: String(data.B ?? ""),
            a,
            A: String(data.A ?? ""),
          });
        }
        return;
      }
      if (kind === "depthSnapshot") {
        const outcome = bookRef.current.applySnapshot(data as never);
        noteRef.current = outcome.note;
        return;
      }
      if (kind === "depthUpdate") {
        const outcome = bookRef.current.applyUpdate(data as never);
        if (outcome.note) noteRef.current = outcome.note;
        return;
      }
      if (kind === "trade") {
        const trade = data as unknown as TradeFrame;
        // The engine stamps the taker's intake time and it travels in the log;
        // there is no client-side clock fallback, so a frame without one is a
        // bug, not something to paper over with `Date.now()`.
        const ts = Number(trade.T ?? trade.E ?? 0);
        if (!Number.isFinite(ts) || ts <= 0) return;
        const stamp = new Date(ts);
        tradeSeq.current += 1;
        tradesRef.current = [
          {
            id: tradeSeq.current,
            time: stamp.toLocaleTimeString([], {
              hour: "2-digit",
              minute: "2-digit",
              second: "2-digit",
              hourCycle: "h23",
              timeZone: "UTC",
            }),
            title: `${stamp.toISOString()} (UTC)`,
            side: (trade.m ? "SELL" : "BUY") as "BUY" | "SELL",
            price: trade.p,
            qty: trade.q,
          },
          ...tradesRef.current,
        ].slice(0, MAX_TRADES);
        barsRef.current.addTrade(ts, Number(trade.p), Number(trade.q));
        return;
      }
      if (kind === "kline") {
        const k = (data as { k?: Record<string, unknown> }).k;
        if (!k) return;
        const tick: KlineTick = {
          time: Math.floor(Number(k.t) / 1000),
          open: Number(k.o),
          high: Number(k.h),
          low: Number(k.l),
          close: Number(k.c),
          volume: Number(k.v),
          interval: String(k.i ?? ""),
          closed: Boolean(k.x),
        };
        if ([tick.time, tick.open, tick.high, tick.low, tick.close].every(Number.isFinite)) {
          barsRef.current.applyServerBar(tick.interval, tick);
        }
        return;
      }
      if (kind === "status") {
        statusRef.current = data as unknown as StatusFrame;
        return;
      }
      if (kind === "resync") {
        const outcome = bookRef.current.markStale(String(data.reason ?? "gap"));
        noteRef.current = outcome.note;
      }
    });

    feed.connect();
    return () => {
      offFrames();
      offStatus();
      feed.disconnect();
    };
  }, []);

  // ---- flush accumulated frame state into React at ~10 fps ----
  useEffect(() => {
    const timer = setInterval(() => {
      const view = bookRef.current.view(DEPTH_ROWS);
      // The assembled ladder is the authoritative BBO once it has levels
      // (bookTicker only fires on top-of-book changes, so a quiet book would
      // otherwise show "—"); before the first snapshot, bookTicker wins.
      if (view.bids.length || view.asks.length) {
        const best = bookRef.current.best();
        if (best.bid || best.ask) bboRef.current = best;
      }
      setBbo({ ...bboRef.current });
      setBook(view);
      setBookNote(noteRef.current);
      setTrades([...tradesRef.current]);
      setFrameCount(frameCounter.current);
      for (const entry of barHandlers.current) entry.handler(barsRef.current.series(entry.interval));
      if (statusRef.current !== statusFrame) setStatusFrame(statusRef.current);
    }, FLUSH_MS);
    return () => clearInterval(timer);
  }, [statusFrame]);

  // ---- reset accumulated state on a symbol change ----
  // Interval changes must not touch the book/tape: the service keeps the depth
  // subscription and would not re-send a snapshot, leaving a blank ladder.
  useEffect(() => {
    symbolRef.current = symbol;
    if (!symbol) return;
    bookRef.current.reset();
    bboRef.current = { bid: null, bidSize: null, ask: null, askSize: null };
    tradesRef.current = [];
    barsRef.current.reset();
    statusRef.current = null;
    noteRef.current = "Waiting for depth.";
    frameCounter.current = 0;
    setFrameCount(0);
    setTrades([]);
    setStatusFrame(null);
    setHistoryCount(0);
    setHistoryError(null);
  }, [symbol]);

  // ---- seed the chart from TDengine before/around live frames ----
  // The market-data service reads the shared historical store behind the same
  // origin proxy, so a cold page load paints real candles without waiting for
  // the next trade. Live frames and the trade tape then overwrite the open bar.
  useEffect(() => {
    if (!symbol) return;
    const controller = new AbortController();
    (async () => {
      try {
        const response = await fetch(
          `/md/klines?symbol=${encodeURIComponent(symbol)}&interval=${encodeURIComponent(interval_)}&limit=500`,
          { signal: controller.signal },
        );
        if (!response.ok) {
          if (!controller.signal.aborted) setHistoryError(`history ${response.status}`);
          return;
        }
        const rows = (await response.json()) as unknown;
        if (controller.signal.aborted || !Array.isArray(rows)) return;
        const bars: Bar[] = [];
        for (const row of rows) {
          if (!Array.isArray(row) || row.length < 12) continue;
          const openMs = Number(row[0]);
          const values = [row[1], row[2], row[3], row[4], row[5]].map(Number);
          if (!Number.isFinite(openMs) || openMs <= 0 || !values.every(Number.isFinite)) continue;
          bars.push({
            time: Math.floor(openMs / 1_000),
            open: values[0],
            high: values[1],
            low: values[2],
            close: values[3],
            volume: values[4],
            closed: String(row[11]) === "1",
          });
        }
        barsRef.current.applyHistoryBars(interval_, bars);
        setHistoryCount(bars.length);
        setHistoryError(null);
        for (const entry of barHandlers.current) {
          entry.handler(barsRef.current.series(entry.interval));
        }
      } catch (error) {
        if (!controller.signal.aborted) {
          setHistoryError(error instanceof Error ? error.message : "history unavailable");
        }
      }
    })();
    return () => controller.abort();
  }, [symbol, interval_]);

  // ---- (re)subscribe when symbol, interval or the stream catalogue changes ----
  useEffect(() => {
    intervalRef.current = interval_;
    if (!symbol || !feedRef.current) return;
    const advertised = new Set(info?.streams ?? []);
    const streams: string[] = [];
    for (const stream of TERMINAL_STREAMS) {
      if (stream === "kline_1s" || stream === "kline_1m") {
        if (stream === `kline_${interval_}` && (!advertised.size || advertised.has(stream))) {
          streams.push(stream);
        }
        continue;
      }
      if (!advertised.size || advertised.has(stream)) streams.push(stream);
    }
    feedRef.current.setWanted(streams.map((stream) => streamName(symbol, stream)));
  }, [symbol, interval_, info]);

  const setSymbol = useCallback((next: string) => {
    setSymbolState(next);
  }, []);

  const setInterval_ = useCallback((next: KlineInterval) => {
    setIntervalState(next);
  }, []);

  const subscribeBars = useCallback((interval: BarInterval, handler: (bars: Bar[]) => void) => {
    const entry = { interval, handler };
    barHandlers.current.add(entry);
    // Seed immediately so a remount (interval toggle, StrictMode) paints the
    // windows already derived from the tape instead of waiting for a flush.
    handler(barsRef.current.series(interval));
    return () => {
      barHandlers.current.delete(entry);
    };
  }, []);

  const value = useMemo<MarketFeedState>(
    () => ({
      symbols: info?.symbols ?? [],
      advertised: info?.streams ?? [],
      fastOnly: info?.fast_only ?? false,
      symbol,
      setSymbol,
      interval: interval_,
      setInterval_,
      status,
      connDetail,
      frameCount,
      bbo,
      book,
      bookNote,
      trades,
      statusFrame,
      subscribeBars,
      ready,
      historyCount,
      historyError,
    }),
    [
      info, symbol, setSymbol, interval_, setInterval_, status, connDetail,
      frameCount, bbo, book, bookNote, trades, statusFrame, subscribeBars, ready,
      historyCount, historyError,
    ],
  );

  return <MarketFeedContext.Provider value={value}>{children}</MarketFeedContext.Provider>;
}

export function useMarketFeed(): MarketFeedState {
  const context = useContext(MarketFeedContext);
  if (!context) throw new Error("useMarketFeed requires MarketFeedProvider");
  return context;
}
