import type { TapeEntry } from "../hooks/useMarketFeed";

export function Trades({ trades }: { trades: TapeEntry[] }) {
  return (
    <div className="panel trades-panel">
      <div className="panel-head">
        <h2>Trades</h2>
        <span className="muted">{trades.length} recent</span>
      </div>
      <table className="tape-table">
        <thead>
          <tr>
            <th>Time (UTC)</th>
            <th>Side</th>
            <th className="num">Price</th>
            <th className="num">Qty</th>
          </tr>
        </thead>
        <tbody>
          {trades.length === 0 ? (
            <tr>
              <td colSpan={4} className="empty">no trades yet</td>
            </tr>
          ) : (
            trades.map((trade) => (
              <tr key={trade.id}>
                <td className="muted" title={trade.title}>{trade.time}</td>
                <td className={trade.side === "BUY" ? "buy" : "sell"}>{trade.side}</td>
                <td className="num">{trade.price}</td>
                <td className="num">{trade.qty}</td>
              </tr>
            ))
          )}
        </tbody>
      </table>
    </div>
  );
}
