// Order book ladder, anchored on the spread like the reference terminal.
//
// The asks and bids are two independently anchored halves: the asks fill
// upwards from the spread (best ask immediately above the mid row) and the
// bids fill downwards from it (best bid immediately below). Each half gets an
// equal share of the body height, so the spread row sits in the vertical
// middle of the panel even when one side is thin or empty, and a deep ladder
// clips its worst levels rather than pushing the mid row off centre.
//
// Each row paints its cumulative depth bar from the right, summed from the
// spread outward, and clicking a level fills the ticket price.

import type { BookView, Bbo } from "../lib/book";

interface Props {
  book: BookView;
  bbo: Bbo;
  note: string;
  quoteAsset: string;
  baseAsset: string;
  lastTrade: { price: string; side: "BUY" | "SELL" | null } | null;
  onPrice: (price: string) => void;
}

function trim(value: number): string {
  if (!Number.isFinite(value)) return "—";
  return value.toFixed(6).replace(/(\.\d*?)0+$/, "$1").replace(/\.$/, "");
}

interface Row {
  price: string;
  size: string;
  cumulative: number;
}

/**
 * Cumulative size from the spread outward, in spread→deep order. The exchange
 * feed sends both sides best-price-first, so one pass suffices; the caller
 * reverses the ask rows for display.
 */
function withCumulative(levels: [string, string][]): Row[] {
  let running = 0;
  return levels.map(([price, size]) => {
    running += Number(size) || 0;
    return { price, size, cumulative: running };
  });
}

export function OrderBook({ book, bbo, note, quoteAsset, baseAsset, lastTrade, onPrice }: Props) {
  // Display order: worst ask at the top, best ask nearest the spread.
  const askRows = withCumulative(book.asks);
  const asks = [...askRows].reverse();
  // Display order: best bid nearest the spread, worst at the bottom.
  const bids = withCumulative(book.bids);

  const spread = bbo.bid !== null && bbo.ask !== null ? Number(bbo.ask) - Number(bbo.bid) : null;
  const maxCumulative = Math.max(bids.at(-1)?.cumulative ?? 0, askRows.at(-1)?.cumulative ?? 0, 0);

  const levelRow = (row: Row, side: "bid" | "ask") => (
    <tr
      key={`${side}-${row.price}`}
      className={`level ${side}`}
      onClick={() => onPrice(row.price)}
      title={`${side === "ask" ? "Sell" : "Buy"} ${baseAsset} at ${row.price}`}
    >
      <td className="num">{row.price}</td>
      <td className="num">{row.size}</td>
      <td
        className="num cum"
        style={
          {
            "--depth": `${maxCumulative > 0 ? Math.min(100, (row.cumulative / maxCumulative) * 100).toFixed(1) : "0"}%`,
          } as React.CSSProperties
        }
      >
        {trim(row.cumulative)}
      </td>
    </tr>
  );

  const side = (rows: Row[], kind: "ask" | "bid") => (
    <div className={`book-side ${kind === "ask" ? "asks" : "bids"}`}>
      {rows.length ? (
        <table className="book-table">
          <colgroup>
            <col className="c-price" />
            <col className="c-size" />
            <col className="c-total" />
          </colgroup>
          <tbody>{rows.map((row) => levelRow(row, kind))}</tbody>
        </table>
      ) : null}
    </div>
  );

  return (
    <div className="panel book-panel">
      <div className="panel-head">
        <h2>Order book</h2>
        <span className={`badge ${book.stale ? "warn" : book.source ? "good" : "neutral"}`}>
          {book.stale ? "stale" : book.source ?? "no data"}
        </span>
      </div>
      <div className="book-head">
        <span className="num">Price ({quoteAsset})</span>
        <span className="num">Size ({baseAsset})</span>
        <span className="num">Total</span>
      </div>
      <div className="book-body">
        {side(asks, "ask")}
        <div className="book-mid">
          <span className={`mid-price ${lastTrade?.side === "SELL" ? "sell" : lastTrade?.side === "BUY" ? "buy" : ""}`}>
            {lastTrade?.price ?? "—"}
          </span>
          <span className="spread-meta">
            {spread !== null
              ? `spread ${trim(spread)} · mid ${trim((Number(bbo.ask) + Number(bbo.bid)) / 2)}`
              : lastTrade
                ? "last trade"
                : ""}
          </span>
        </div>
        {side(bids, "bid")}
      </div>
      {bids.length === 0 && asks.length === 0 ? (
        <div className={`book-empty${book.stale ? " warn" : ""}`}>
          {book.stale ? "Depth stale; waiting for a fresh snapshot." : note || "No resting levels."}
        </div>
      ) : null}
      <div className={`book-note${book.stale ? " warn" : ""}`}>{note}</div>
    </div>
  );
}
