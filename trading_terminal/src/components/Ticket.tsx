// Buy/sell ticket in the reference terminal's arrangement: half-tone
// Buy/Sell tabs on top, order-type row, price/amount with quick fills, an
// "Advanced" disclosure for TIF/post-only/iceberg/stop/fees, the freeze
// estimate, then the action button. Orders go through the order-system BFF;
// the engine is never called from the browser.

import { useEffect, useMemo, useState } from "react";
import { newClientId } from "../lib/desk";
import { useFreezePreview } from "../hooks/useFreezePreview";
import { useMarketFeed } from "../hooks/useMarketFeed";
import type { DeskSnapshot } from "../hooks/useDesk";
import type { OrderType, PlaceBody, Side, Tif, TicketForm, VolumeUnit } from "../lib/types";

interface Props {
  desk: DeskSnapshot;
  userId: number;
  onUserId: (id: number) => void;
  pendingPrice: string | null;
  onConsumePrice: () => void;
}

const DEFAULT_FORM: TicketForm = {
  side: "Buy",
  type: "Limit",
  tif: "GTC",
  price: "",
  volume: "",
  volumeUnit: "base",
  postOnly: false,
  iceberg: false,
  displayVolume: "",
  stopPrice: "",
  expiry: "",
  makerFeeRate: "",
  takerFeeRate: "",
  leverage: 1,
};

function buildPlaceBody(
  form: TicketForm,
  userId: number,
  target: string,
  withClientId: boolean,
): { body?: PlaceBody; error?: string } {
  const volume = form.volume.trim();
  if (!volume) return { error: "Volume is required" };
  const price = form.price.trim();
  if (form.type === "Limit" && !price) return { error: "Price is required for a limit order" };
  const leverage = Math.floor(form.leverage);
  if (!Number.isFinite(leverage) || leverage < 1 || leverage > 100) {
    return { error: "Leverage must be between 1 and 100" };
  }
  const body: PlaceBody = {
    side: form.side,
    user_id: userId,
    target,
    type: form.type,
    volume,
    volume_unit: form.volumeUnit,
    maker_fee_rate: form.makerFeeRate.trim() || "0",
    taker_fee_rate: form.takerFeeRate.trim() || "0",
    leverage,
  };
  if (form.tif) body.tif = form.tif;
  if (form.type === "Limit") {
    body.price = price;
    if (form.postOnly) body.post_only = true;
  }
  const stop = form.stopPrice.trim();
  if (stop) body.stop_price = stop;
  if (form.iceberg && form.type === "Limit") {
    const display = form.displayVolume.trim();
    if (display) body.display_volume = display;
  }
  if (form.tif === "GTD") {
    const expiry = Date.parse(form.expiry);
    if (!Number.isFinite(expiry)) return { error: "GTD expiry is required" };
    body.expiry_ms = expiry;
  }
  if (withClientId) body.client_order_id = newClientId();
  return { body };
}

function trim(value: number): string {
  if (!Number.isFinite(value)) return "—";
  return value.toFixed(6).replace(/(\.\d*?)0+$/, "$1").replace(/\.$/, "");
}

function percent(rate: string): string {
  const value = Number(rate);
  if (!Number.isFinite(value)) return rate;
  return `${Number((value * 100).toFixed(6))}%`;
}

export function Ticket({ desk, userId, onUserId, pendingPrice, onConsumePrice }: Props) {
  const { bbo, symbol } = useMarketFeed();
  const [form, setForm] = useState<TicketForm>(DEFAULT_FORM);

  const config = desk.state?.symbol.config ?? null;
  const isPerp = (desk.backends?.targets ?? []).find((entry) => entry.id === (symbol ?? ""))?.product === "perpetual";
  const target = symbol ?? "";

  // Clicking a book level fills the limit price.
  useEffect(() => {
    if (pendingPrice === null) return;
    setForm((previous) => ({ ...previous, type: previous.type === "Market" ? "Limit" : previous.type, price: pendingPrice }));
    onConsumePrice();
  }, [pendingPrice, onConsumePrice]);

  const update = <K extends keyof TicketForm>(key: K, value: TicketForm[K]) =>
    setForm((previous) => ({ ...previous, [key]: value }));

  const previewBody = useMemo(() => {
    if (!target || !userId || userId < 0) return null;
    const built = buildPlaceBody(form, userId, target, false);
    return built.body ?? null;
  }, [form, userId, target]);

  const preview = useFreezePreview(previewBody, Boolean(config));

  const [submitting, setSubmitting] = useState(false);
  const [result, setResult] = useState<{ ok: boolean; title: string; text: string } | null>(null);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!target) return;
    const built = buildPlaceBody(form, userId, target, true);
    if (built.error || !built.body) {
      setResult({ ok: false, title: "Invalid ticket", text: built.error ?? "invalid ticket" });
      return;
    }
    setSubmitting(true);
    try {
      const response = await desk.place(built.body);
      const rejected = (response.events ?? []).some((event) => event.kind === "rejected");
      const sequence = response.sequence ?? "—";
      const detail =
        response.error ??
        (response.events ?? []).find((event) => event.kind === "rejected")?.reason ??
        `${response.events?.length ?? 0} event(s) · log_index ${response.log_index ?? "—"}`;
      setResult({
        ok: !rejected && !response.error,
        title: `${rejected || response.error ? "Rejected" : "Accepted"} · sequence ${sequence}`,
        text: detail,
      });
    } catch (error) {
      setResult({ ok: false, title: "Rejected", text: error instanceof Error ? error.message : String(error) });
    } finally {
      setSubmitting(false);
    }
  };

  const market = form.type === "Market";
  const quote = config?.quote_asset.code ?? "quote";
  const base = config?.base_asset.code ?? "base";
  const total =
    form.price && form.volume && form.volumeUnit === "base"
      ? Number(form.price) * Number(form.volume)
      : form.volumeUnit === "quote" && form.volume
        ? Number(form.volume)
        : null;

  return (
    <div className="panel ticket-panel">
      <div className="ticket-account">
        <label>
          user
          <input
            type="number"
            min={0}
            value={Number.isNaN(userId) ? 0 : userId}
            onChange={(event) => onUserId(Math.max(0, Math.floor(Number(event.target.value) || 0)))}
          />
        </label>
        <span className="muted small">{isPerp ? "Cross · demo perp" : "Spot"}</span>
      </div>

      <div className="side-tabs">
        <button
          type="button"
          className={`side-tab buy ${form.side === "Buy" ? "active" : ""}`}
          onClick={() => update("side", "Buy" as Side)}
        >
          Buy{isPerp ? " / Long" : ""}
        </button>
        <button
          type="button"
          className={`side-tab sell ${form.side === "Sell" ? "active" : ""}`}
          onClick={() => update("side", "Sell" as Side)}
        >
          Sell{isPerp ? " / Short" : ""}
        </button>
      </div>

      <form onSubmit={submit} className="ticket-form">
        <div className="type-row">
          <select value={form.type} onChange={(event) => update("type", event.target.value as OrderType)}>
            <option value="Limit">Limit</option>
            <option value="Market">Market</option>
          </select>
          <span className="type-hint">{isPerp ? `${form.leverage}x` : "Spot"}</span>
        </div>

        {isPerp ? (
          <label className="line-field">
            <span className="line-label">Leverage</span>
            <input
              type="number"
              min={1}
              max={100}
              step={1}
              value={form.leverage}
              onChange={(event) => {
                const next = Number(event.target.value);
                update("leverage", Number.isFinite(next) ? Math.min(100, Math.max(1, Math.floor(next))) : 1);
              }}
            />
            <span className="line-unit">x</span>
          </label>
        ) : null}

        {market ? (
          <div className="market-note muted small">
            Executes immediately against the book; the unfilled remainder is cancelled.
          </div>
        ) : (
          <label className="line-field">
            <span className="line-label">Price</span>
            <input
              value={form.price}
              placeholder={config?.price_tick ?? ""}
              inputMode="decimal"
              onChange={(event) => update("price", event.target.value)}
            />
            <span className="line-unit">{quote}</span>
          </label>
        )}
        {!market ? (
          <div className="quick-buttons">
            <button type="button" disabled={!bbo.bid} onClick={() => update("price", bbo.bid ?? "")}>Bid</button>
            <button type="button" disabled={!bbo.ask} onClick={() => update("price", bbo.ask ?? "")}>Ask</button>
            <button
              type="button"
              disabled={!bbo.bid || !bbo.ask}
              onClick={() => update("price", String((Number(bbo.bid) + Number(bbo.ask)) / 2))}
            >
              Mid
            </button>
          </div>
        ) : null}

        <label className="line-field">
          <span className="line-label">Amount</span>
          <input
            value={form.volume}
            placeholder={config?.base_lot ?? ""}
            inputMode="decimal"
            onChange={(event) => update("volume", event.target.value)}
          />
          <span className="line-unit">
            <select value={form.volumeUnit} onChange={(event) => update("volumeUnit", event.target.value as VolumeUnit)}>
              <option value="base">{base}</option>
              <option value="quote">{quote}</option>
            </select>
          </span>
        </label>
        {total !== null && Number.isFinite(total) ? (
          <div className="total-row">
            <span className="muted small">Total</span>
            <span className="num">{trim(total)} {quote}</span>
          </div>
        ) : null}

        <details className="advanced">
          <summary className="muted small">Advanced — TIF, post-only, iceberg, stop, fees</summary>
          <div className="advanced-body">
            <label className="line-field">
              <span className="line-label">TIF</span>
              <select value={form.tif} onChange={(event) => update("tif", event.target.value as Tif)}>
                <option value="GTC">GTC</option>
                <option value="IOC">IOC</option>
                <option value="FOK">FOK</option>
                <option value="GTD">GTD</option>
              </select>
              <span className="line-unit" />
            </label>
            {form.tif === "GTD" ? (
              <label className="line-field">
                <span className="line-label">GTD expiry</span>
                <input type="datetime-local" value={form.expiry} onChange={(event) => update("expiry", event.target.value)} />
                <span className="line-unit" />
              </label>
            ) : null}
            {form.type === "Limit" ? (
              <>
                <label className="check">
                  <input type="checkbox" checked={form.postOnly} onChange={(event) => update("postOnly", event.target.checked)} />
                  Post-only (maker only, refuses to cross)
                </label>
                <label className="check">
                  <input type="checkbox" checked={form.iceberg} onChange={(event) => update("iceberg", event.target.checked)} />
                  Iceberg (show only the display tranche)
                </label>
                {form.iceberg ? (
                  <label className="line-field">
                    <span className="line-label">Display tranche</span>
                    <input value={form.displayVolume} inputMode="decimal" onChange={(event) => update("displayVolume", event.target.value)} />
                    <span className="line-unit">{base}</span>
                  </label>
                ) : null}
              </>
            ) : null}
            <label className="line-field">
              <span className="line-label">Stop trigger</span>
              <input
                value={form.stopPrice}
                inputMode="decimal"
                placeholder="0 = not a stop"
                onChange={(event) => update("stopPrice", event.target.value)}
              />
              <span className="line-unit">{quote}</span>
            </label>
            <div className="fee-grid">
              <label className="line-field">
                <span className="line-label">Maker fee</span>
                <input value={form.makerFeeRate} placeholder="0" inputMode="decimal" onChange={(event) => update("makerFeeRate", event.target.value)} />
                <span className="line-unit" />
              </label>
              <label className="line-field">
                <span className="line-label">Taker fee</span>
                <input value={form.takerFeeRate} placeholder="0" inputMode="decimal" onChange={(event) => update("takerFeeRate", event.target.value)} />
                <span className="line-unit" />
              </label>
            </div>
          </div>
        </details>

        <div className="freeze-preview">
          <div className="freeze-row">
            <span className="muted small">Estimated freeze</span>
            <span className="freeze-value mono">
              {preview.freeze ? `${preview.freeze.amount} ${preview.freeze.asset}` : "—"}
            </span>
          </div>
          <div className="muted small">
            {preview.error ??
              (preview.freeze
                ? `${preview.maker ? "worst-case maker" : "taker"} · ${preview.freeze.basis} · fee ${percent(preview.freeze.fee_rate)} · ref ${preview.freeze.reference_price}`
                : "Fill the ticket to preview the order-system freeze.")}
          </div>
          <div className="muted small">demo estimate · engine holds no balances</div>
        </div>

        <button type="submit" className={`submit ${form.side === "Buy" ? "buy" : "sell"}`} disabled={submitting}>
          {submitting ? "Submitting…" : `${form.side === "Buy" ? "Buy" : "Sell"} ${base}${isPerp ? ` · ${form.leverage}x` : ""}`}
        </button>
      </form>

      {result ? (
        <div className={`result-banner ${result.ok ? "good" : "bad"}`}>
          <strong>{result.title}</strong>
          <span>{result.text}</span>
        </div>
      ) : null}
    </div>
  );
}
