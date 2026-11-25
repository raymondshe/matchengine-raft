import { useMarketFeed } from "../hooks/useMarketFeed";

function trim(value: number): string {
  if (!Number.isFinite(value)) return "—";
  return value.toFixed(6).replace(/(\.\d*?)0+$/, "$1").replace(/\.$/, "");
}

export function Header() {
  const { symbols, symbol, setSymbol, status, connDetail, bbo, ready, frameCount, statusFrame, trades } =
    useMarketFeed();

  const info = symbols.find((entry) => entry.symbol === symbol);
  const symbolReady = ready?.symbols?.[symbol ?? ""]?.ready ?? ready?.ready ?? false;
  const spread = bbo.bid !== null && bbo.ask !== null ? Number(bbo.ask) - Number(bbo.bid) : null;
  const mid = bbo.bid !== null && bbo.ask !== null ? (Number(bbo.ask) + Number(bbo.bid)) / 2 : null;

  // Last trade and book sequence come from the market-data feed (trade tape
  // and STATUS frames), never from the order-system REST poll.
  const lastTrade = trades[0]?.price ?? null;
  const lastSide = trades[0]?.side;
  const priceTone = lastSide === "SELL" ? "sell" : "buy";

  const connLabel = status === "connected" ? "live" : status === "connecting" ? "connecting" : status;
  const connTone = status === "connected" ? "good" : status === "connecting" ? "warn" : "bad";

  return (
    <header className="terminal-header">
      <div className="brand">
        <span className="brand-mark">match</span>
        <span className="brand-rest">terminal</span>
        <span className="badge">SPOT</span>
      </div>
      <div className="pair-block">
        <select
          className="pair-select"
          value={symbol ?? ""}
          onChange={(event) => setSymbol(event.target.value)}
          aria-label="Market"
        >
          {symbols.map((entry) => (
            <option key={entry.symbol} value={entry.symbol}>
              {entry.symbol.toUpperCase()}/{entry.quote_asset}
            </option>
          ))}
        </select>
        <div className="pair-meta muted">{info ? `id ${info.symbol_id}` : ""}</div>
      </div>
      <div className="price-block">
        <span className={`last-price ${lastTrade ? priceTone : ""}`}>{lastTrade ?? (mid ? trim(mid) : "—")}</span>
        <span className="price-caption muted">
          {lastTrade ? `last trade · ${quoteOf(info)}` : "no trade yet"}
        </span>
      </div>
      <div className="stat-block">
        <div className="stat">
          <span className="stat-label">Bid</span>
          <span className="stat-value buy">{bbo.bid ?? "—"}</span>
        </div>
        <div className="stat">
          <span className="stat-label">Ask</span>
          <span className="stat-value sell">{bbo.ask ?? "—"}</span>
        </div>
        <div className="stat">
          <span className="stat-label">Spread</span>
          <span className="stat-value">{spread === null ? "—" : trim(spread)}</span>
        </div>
        <div className="stat">
          <span className="stat-label">Book seq</span>
          <span className="stat-value">{statusFrame?.bookSequence ?? "—"}</span>
        </div>
      </div>
      <div className="header-chips">
        <span className={`chip ${connTone}`}>
          <span className={`dot ${connTone}`} />
          {connLabel}
        </span>
        <span className={`chip ${symbolReady ? "good" : "warn"}`}>
          <span className={`dot ${symbolReady ? "good" : "warn"}`} />
          feed {symbolReady ? "ready" : "waiting"}
        </span>
        <span className="chip neutral">{frameCount.toLocaleString()} frames</span>
        {connDetail ? <span className="chip warn" title={connDetail}>detail</span> : null}
      </div>
    </header>
  );
}

function quoteOf(info: { quote_asset: string } | undefined): string {
  return info ? `in ${info.quote_asset}` : "";
}
