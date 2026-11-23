// Open orders, parked stops and the fill tape. Cancel is one click; amend
// opens an inline editor (the desk's window.prompt replaced by a form) that
// carries the engine's amend rules: same-price open-volume reductions keep
// time priority, a new price or larger open volume re-inserts under a new
// sequence, and crossing amendments are refused.

import { useState } from "react";
import type { DeskSnapshot } from "../hooks/useDesk";
import type { DeskEvent, DeskOrderEntry } from "../lib/types";

type Tab = "orders" | "stops" | "fills";

export function OrdersPanel({ desk }: { desk: DeskSnapshot }) {
  const [tab, setTab] = useState<Tab>("orders");
  const [amendEntry, setAmendEntry] = useState<DeskOrderEntry | null>(null);
  const state = desk.state;

  const orders = state?.orders.items ?? [];
  const stops = state?.stops.items ?? [];

  return (
    <div className="panel orders-panel">
      <div className="panel-head">
        <div className="tabs" role="tablist">
          <button type="button" className={tab === "orders" ? "active" : ""} onClick={() => setTab("orders")}>
            Open orders <span className="count">{state?.orders.total ?? orders.length}</span>
          </button>
          <button type="button" className={tab === "stops" ? "active" : ""} onClick={() => setTab("stops")}>
            Parked stops <span className="count">{stops.length}</span>
          </button>
          <button type="button" className={tab === "fills" ? "active" : ""} onClick={() => setTab("fills")}>
            Fills & events <span className="count">{desk.events.length}</span>
          </button>
        </div>
        {tab === "fills" ? (
          <div className="event-controls">
            <span className="muted small">
              {desk.cursor ? `cursor ${desk.cursor.log_index}:${desk.cursor.ordinal}` : "waiting for events"}
            </span>
            <button type="button" onClick={desk.togglePause}>{desk.paused ? "Resume" : "Pause"}</button>
            <button type="button" onClick={desk.resetEvents}>Reset</button>
          </div>
        ) : null}
      </div>

      {tab === "orders" ? (
        <OrdersTable entries={orders} onCancel={desk.cancelOrder} onAmend={setAmendEntry} empty="No resting orders" />
      ) : null}
      {tab === "stops" ? (
        <OrdersTable entries={stops} onCancel={desk.cancelOrder} onAmend={setAmendEntry} empty="No parked stops" stops />
      ) : null}
      {tab === "fills" ? <FillsTable events={desk.events} /> : null}

      {amendEntry ? (
        <AmendForm
          entry={amendEntry}
          onCancel={() => setAmendEntry(null)}
          onSubmit={async (price, volume) => {
            await desk.amendOrder(amendEntry, price, volume);
            setAmendEntry(null);
          }}
        />
      ) : null}
    </div>
  );
}

function OrdersTable({
  entries,
  onCancel,
  onAmend,
  empty,
  stops = false,
}: {
  entries: DeskOrderEntry[];
  onCancel: (entry: DeskOrderEntry) => Promise<unknown>;
  onAmend: (entry: DeskOrderEntry) => void;
  empty: string;
  stops?: boolean;
}) {
  const [pending, setPending] = useState<number | null>(null);
  return (
    <table className="data-table">
      <thead>
        <tr>
          <th>Seq</th>
          <th>Side</th>
          <th className="num">{stops ? "Trigger" : "Price"}</th>
          <th className="num">Volume</th>
          <th className="num">Freeze</th>
          <th className="actions-cell" />
        </tr>
      </thead>
      <tbody>
        {entries.length === 0 ? (
          <tr>
            <td colSpan={6} className="empty">{empty}</td>
          </tr>
        ) : (
          entries.map((entry) => {
            const order = entry.order;
            const freeze = entry.economics?.freeze;
            const amendable = order.live === "Limit";
            return (
              <tr key={`${order.side}-${order.sequence}`} title={typeLabel(order)}>
                <td className="mono">{order.sequence}</td>
                <td className={order.side === "Sell" ? "sell" : "buy"}>{order.side}</td>
                <td className="num mono">{stops ? order.stop_price : order.price}</td>
                <td className="num mono">{order.volume}</td>
                <td className="num mono">{freeze ? `${freeze.amount} ${freeze.asset}` : "—"}</td>
                <td className="actions-cell">
                  <div className="actions">
                    <button
                      type="button"
                      className="ghost"
                      disabled={pending === order.sequence}
                      onClick={async () => {
                        setPending(order.sequence);
                        try {
                          await onCancel(entry);
                        } finally {
                          setPending(null);
                        }
                      }}
                    >
                      {pending === order.sequence ? "…" : "Cancel"}
                    </button>
                    {amendable ? <button type="button" onClick={() => onAmend(entry)}>Amend</button> : null}
                  </div>
                </td>
              </tr>
            );
          })
        )}
      </tbody>
    </table>
  );
}

function FillsTable({ events }: { events: DeskEvent[] }) {
  const visible = events.slice(-100).reverse();
  return (
    <table className="data-table">
      <thead>
        <tr>
          <th>Event</th>
          <th>Kind</th>
          <th>Side</th>
          <th className="num">Order</th>
          <th className="num">Price</th>
          <th className="num">Volume</th>
          <th className="num">Fees</th>
          <th>Reason</th>
        </tr>
      </thead>
      <tbody>
        {visible.length === 0 ? (
          <tr>
            <td colSpan={8} className="empty">No events in the retained window</td>
          </tr>
        ) : (
          visible.map((event) => (
            <tr key={`${event.event_id.log_index}:${event.event_id.ordinal}`}>
              <td className="mono muted">{event.event_id.log_index}:{event.event_id.ordinal}</td>
              <td>
                <span className={`badge ${event.kind === "fill" ? "good" : event.kind === "rejected" ? "bad" : event.kind === "cancelled" || event.kind === "expired" ? "warn" : "neutral"}`}>
                  {event.kind}
                </span>
              </td>
              <td className={event.side === "Sell" ? "sell" : "buy"}>{event.side}</td>
              <td className="num mono">{event.order_sequence}</td>
              <td className="num mono">{event.price}</td>
              <td className="num mono">{event.volume}</td>
              <td className="num small">
                {event.kind === "fill"
                  ? `t ${event.taker_fee} ${event.taker_fee_asset ?? ""} / m ${event.maker_fee} ${event.maker_fee_asset ?? ""}`
                  : "—"}
              </td>
              <td className="mono small">{event.reason ?? event.error?.join(", ") ?? ""}</td>
            </tr>
          ))
        )}
      </tbody>
    </table>
  );
}

function AmendForm({
  entry,
  onCancel,
  onSubmit,
}: {
  entry: DeskOrderEntry;
  onCancel: () => void;
  onSubmit: (price: string | null, volume: string | null) => Promise<void>;
}) {
  const [price, setPrice] = useState("");
  const [volume, setVolume] = useState("");
  const [busy, setBusy] = useState(false);
  return (
    <form
      className="amend-form"
      onSubmit={async (event) => {
        event.preventDefault();
        if (!price.trim() && !volume.trim()) return;
        setBusy(true);
        try {
          await onSubmit(price.trim() || null, volume.trim() || null);
        } finally {
          setBusy(false);
        }
      }}
    >
      <span className="muted small">
        Amend order {entry.order.sequence} ({entry.order.side} {entry.order.volume} @ {entry.order.price}) — leave a
        field blank to keep it
      </span>
      <input placeholder={`New price (${entry.order.price})`} value={price} inputMode="decimal" onChange={(event) => setPrice(event.target.value)} />
      <input placeholder={`New volume (${entry.order.volume})`} value={volume} inputMode="decimal" onChange={(event) => setVolume(event.target.value)} />
      <button type="submit" className="primary" disabled={busy}>{busy ? "Amending…" : "Amend"}</button>
      <button type="button" className="ghost" onClick={onCancel}>Close</button>
    </form>
  );
}

function typeLabel(order: DeskOrderEntry["order"]): string {
  const bits: string[] = [order.live];
  const tif = typeof order.tif === "string" ? order.tif : Object.keys(order.tif ?? {})[0];
  if (tif && tif !== "GTC") bits.push(tif);
  if (order.post_only) bits.push("post-only");
  if (order.display_volume && order.display_volume !== "0") bits.push(`iceberg ${order.display_volume}`);
  if (order.stop_price && order.stop_price !== "0") bits.push(`stop ${order.stop_price}`);
  return bits.join(" · ") || "—";
}
