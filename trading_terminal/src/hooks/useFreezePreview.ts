// Debounced freeze preview for the ticket, ported from the order desk's
// `scheduleTicketPreview`/`refreshTicketPreview` (250 ms debounce + a
// sequence guard so a slow response cannot overwrite a newer one).

import { useEffect, useRef, useState } from "react";
import * as desk from "../lib/desk";
import type { PlaceBody } from "../lib/types";

export interface FreezePreviewState {
  loading: boolean;
  error: string | null;
  freeze: {
    amount: string;
    asset: string;
    role: string;
    basis: string;
    fee_rate: string;
    reference_price: string;
  } | null;
  maker: boolean;
  perpetual: {
    leverage: number;
    position: string;
    notional: string;
    initial_margin: string;
    fee: { amount: string; asset: string };
    buffer: string;
  } | null;
}

const EMPTY: FreezePreviewState = { loading: false, error: null, freeze: null, maker: false, perpetual: null };

export function useFreezePreview(body: PlaceBody | null, enabled: boolean): FreezePreviewState {
  const [preview, setPreview] = useState<FreezePreviewState>(EMPTY);
  const seq = useRef(0);

  useEffect(() => {
    if (!body || !enabled) {
      setPreview(EMPTY);
      return;
    }
    const timer = setTimeout(async () => {
      const id = ++seq.current;
      setPreview((previous) => ({ ...previous, loading: true, error: null }));
      try {
        const response = await desk.freezePreview(body);
        if (id !== seq.current) return;
        const economics = response.economics;
        if (!economics || !economics.freeze) {
          setPreview({ loading: false, error: response.reason ?? "No estimate available.", freeze: null, maker: false, perpetual: null });
          return;
        }
        setPreview({
          loading: false,
          error: null,
          freeze: economics.freeze,
          maker: Boolean(economics.maker),
          perpetual: economics.perpetual ?? null,
        });
      } catch (error) {
        if (id !== seq.current) return;
        setPreview({
          loading: false,
          error: error instanceof Error ? error.message : String(error),
          freeze: null,
          maker: false,
          perpetual: null,
        });
      }
    }, 250);
    return () => clearTimeout(timer);
  }, [body, enabled]);

  return preview;
}
