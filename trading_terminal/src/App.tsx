import { useCallback, useState } from "react";
import { Chart } from "./components/Chart";
import { Header } from "./components/Header";
import { OrderBook } from "./components/OrderBook";
import { OrdersPanel } from "./components/OrdersPanel";
import { Ticket } from "./components/Ticket";
import { Trades } from "./components/Trades";
import { useDesk } from "./hooks/useDesk";
import { useMarketFeed } from "./hooks/useMarketFeed";

// Binance spot-trade-page arrangement:
//   left column   order book over the trade tape
//   center column candlestick chart over the order tabs
//   right column  buy/sell ticket
export function App() {
  const feed = useMarketFeed();
  const [userId, setUserId] = useState(1);
  const desk = useDesk(feed.symbol, userId);
  const [pendingPrice, setPendingPrice] = useState<string | null>(null);

  const onPrice = useCallback((price: string) => setPendingPrice(price), []);
  const consumePrice = useCallback(() => setPendingPrice(null), []);

  const config = desk.state?.symbol.config ?? null;
  // The reference terminal paints the big mid-row number with the last
  // traded price, coloured by the taker side; fall back to the desk's book
  // value (uncoloured) before the first tape print.
  const lastTrade = feed.trades[0]
    ? { price: feed.trades[0].price, side: feed.trades[0].side }
    : desk.state?.book.last_trade_price
      ? { price: desk.state.book.last_trade_price, side: null }
      : null;

  return (
    <div className="terminal">
      <Header deskState={desk.state} />
      {desk.warning ? <div className="warning-banner">{desk.warning}</div> : null}
      <main className="terminal-grid">
        <section className="col market-column">
          <OrderBook
            book={feed.book}
            bbo={feed.bbo}
            note={feed.bookNote}
            quoteAsset={config?.quote_asset.code ?? "quote"}
            baseAsset={config?.base_asset.code ?? "base"}
            lastTrade={lastTrade}
            onPrice={onPrice}
          />
          <Trades trades={feed.trades} />
        </section>
        <section className="col center-column">
          <Chart />
          <OrdersPanel desk={desk} />
        </section>
        <section className="col ticket-column">
          <Ticket
            desk={desk}
            userId={userId}
            onUserId={setUserId}
            pendingPrice={pendingPrice}
            onConsumePrice={consumePrice}
          />
        </section>
      </main>
      <footer className="terminal-footer">
        <span>matchengine terminal · demo</span>
        <span>
          {desk.state?.node
            ? `engine ${desk.state.node.http_addr} · ${desk.state.node.ready ? "ready" : "not ready"}${desk.state.node.is_leader ? "" : ` · follower of ${desk.state.node.leader_addr ?? desk.state.node.leader_id}`}`
            : "connecting to the order desk…"}
        </span>
      </footer>
    </div>
  );
}
