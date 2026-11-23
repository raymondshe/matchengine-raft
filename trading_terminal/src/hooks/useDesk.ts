// Order-desk state: the BFF REST loops from `order_system/web/app.js` ported
// to React. `state` polls every second, the event feed follows the
// `(log_index, ordinal)` cursor every 700 ms with the same reset-on-trim rule,
// and write actions trigger an immediate refresh.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as desk from "../lib/desk";
import type {
  DeskBackends,
  DeskEvent,
  DeskOrderEntry,
  DeskState,
  DeskWriteResult,
  PlaceBody,
} from "../lib/types";

export interface DeskSnapshot {
  backends: DeskBackends | null;
  state: DeskState | null;
  events: DeskEvent[];
  cursor: { log_index: number; ordinal: number } | null;
  paused: boolean;
  warning: string | null;
  togglePause: () => void;
  resetEvents: () => void;
  place: (body: PlaceBody) => Promise<DeskWriteResult>;
  cancelOrder: (entry: DeskOrderEntry) => Promise<DeskWriteResult>;
  amendOrder: (
    entry: DeskOrderEntry,
    newPrice: string | null,
    newVolume: string | null,
  ) => Promise<DeskWriteResult>;
}

const EVENTS_SOFT_CAP = 5000;
const STATE_INTERVAL_MS = 1000;
const EVENT_INTERVAL_MS = 700;
const EVENT_PAGE_LIMIT = 500;

function compareId(
  a: { log_index: number; ordinal: number },
  b: { log_index: number; ordinal: number },
): number {
  return a.log_index - b.log_index || a.ordinal - b.ordinal;
}

function eventKey(id: { log_index: number; ordinal: number }): string {
  return `${id.log_index}:${id.ordinal}`;
}

export function useDesk(symbol: string | null, userId: number): DeskSnapshot {
  const [backends, setBackends] = useState<DeskBackends | null>(null);
  const [state, setState] = useState<DeskState | null>(null);
  const [events, setEvents] = useState<DeskEvent[]>([]);
  const [cursor, setCursor] = useState<{ log_index: number; ordinal: number } | null>(null);
  const [paused, setPaused] = useState(false);
  const [warning, setWarning] = useState<string | null>(null);

  const eventsRef = useRef(new Map<string, DeskEvent>());
  const cursorRef = useRef<{ log_index: number; ordinal: number } | null>(null);
  const pausedRef = useRef(false);
  const loadingRef = useRef({ state: false, events: false });

  // Target id: the BFF addresses engines by target id; the terminal picks the
  // target whose id matches the market symbol, falling back to the symbol.
  const target = useMemo(() => {
    if (!symbol) return "";
    const targets = backends?.targets ?? [];
    const match = targets.find((entry) => entry.id === symbol);
    return match ? match.id : symbol;
  }, [symbol, backends]);

  const applyState = useCallback((next: DeskState) => {
    setState(next);
    if (next.backends) setBackends(next.backends);
    setWarning(null);
  }, []);

  // ---- backends once + whenever the symbol changes before state arrives ----
  useEffect(() => {
    let cancelled = false;
    desk
      .getBackends()
      .then((next) => {
        if (!cancelled) setBackends(next);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  // ---- state poll ----
  useEffect(() => {
    if (!symbol) return;
    let cancelled = false;
    const tick = async () => {
      if (document.hidden || loadingRef.current.state) return;
      loadingRef.current.state = true;
      try {
        const next = await desk.getState(target);
        if (!cancelled) applyState(next);
      } catch (error) {
        if (!cancelled) setWarning(`${symbol}: ${error instanceof Error ? error.message : String(error)}`);
      } finally {
        loadingRef.current.state = false;
      }
    };
    void tick();
    const timer = setInterval(tick, STATE_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [symbol, target, applyState]);

  // ---- event cursor-follow ----
  const pullEvents = useCallback(
    async (reset: boolean) => {
      if (!symbol || pausedRef.current || document.hidden || loadingRef.current.events) return;
      loadingRef.current.events = true;
      try {
        if (reset) {
          eventsRef.current.clear();
          cursorRef.current = null;
          setEvents([]);
          setCursor(null);
        }
        let localCursor = cursorRef.current;
        let resetOnce = false;
        for (let page = 0; page < 20; page += 1) {
          const response = await desk.getEvents(target, localCursor, EVENT_PAGE_LIMIT);
          if (localCursor && response.oldest_event_id && compareId(response.oldest_event_id, localCursor) > 0) {
            if (resetOnce) break;
            resetOnce = true;
            eventsRef.current.clear();
            localCursor = null;
            setWarning("Event history trimmed; reloaded from the oldest event.");
            continue;
          }
          for (const event of response.events) {
            eventsRef.current.set(eventKey(event.event_id), event);
          }
          const next = response.next_after;
          if (next && localCursor && compareId(next, localCursor) === 0) break;
          if (next) localCursor = next;
          if (!response.has_more) break;
        }
        // Prune old entries beyond the soft cap, keep order.
        if (eventsRef.current.size > EVENTS_SOFT_CAP) {
          const excess = eventsRef.current.size - EVENTS_SOFT_CAP;
          const keys = [...eventsRef.current.keys()].slice(0, excess);
          for (const key of keys) eventsRef.current.delete(key);
        }
        cursorRef.current = localCursor;
        setCursor(localCursor);
        setEvents(
          [...eventsRef.current.values()].sort(
            (a, b) => compareId(a.event_id, b.event_id),
          ),
        );
        setWarning(null);
      } catch (error) {
        setWarning(`events: ${error instanceof Error ? error.message : String(error)}`);
      } finally {
        loadingRef.current.events = false;
      }
    },
    [symbol, target],
  );

  useEffect(() => {
    if (!symbol) return;
    void pullEvents(false);
    const timer = setInterval(() => void pullEvents(false), EVENT_INTERVAL_MS);
    return () => clearInterval(timer);
  }, [symbol, pullEvents]);

  const afterWrite = useCallback(() => {
    void (async () => {
      try {
        applyState(await desk.getState(target));
      } catch {
        // the next poll surfaces the failure
      }
      void pullEvents(false);
    })();
  }, [target, applyState, pullEvents]);

  const place = useCallback(
    async (body: PlaceBody) => {
      const result = await desk.place({ ...body, user_id: userId, target });
      afterWrite();
      return result;
    },
    [userId, target, afterWrite],
  );

  const cancelOrder = useCallback(
    async (entry: DeskOrderEntry) => {
      const result = await desk.cancel({
        user_id: userId,
        target,
        side: entry.order.side,
        price: entry.order.price,
        sequence: entry.order.sequence,
      });
      afterWrite();
      return result;
    },
    [userId, target, afterWrite],
  );

  const amendOrder = useCallback(
    async (entry: DeskOrderEntry, newPrice: string | null, newVolume: string | null) => {
      const body: Parameters<typeof desk.amend>[0] = {
        user_id: userId,
        target,
        side: entry.order.side,
        price: entry.order.price,
        sequence: entry.order.sequence,
      };
      if (newPrice) body.new_price = newPrice;
      if (newVolume) body.new_volume = newVolume;
      const result = await desk.amend(body);
      afterWrite();
      return result;
    },
    [userId, target, afterWrite],
  );

  const togglePause = useCallback(() => {
    setPaused((previous) => {
      pausedRef.current = !previous;
      return !previous;
    });
  }, []);

  const resetEvents = useCallback(() => {
    eventsRef.current.clear();
    cursorRef.current = null;
    setEvents([]);
    setCursor(null);
    void pullEvents(true);
  }, [pullEvents]);

  return {
    backends,
    state,
    events,
    cursor,
    paused,
    warning,
    togglePause,
    resetEvents,
    place,
    cancelOrder,
    amendOrder,
  };
}
