import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./App";
import { MarketFeedProvider } from "./hooks/useMarketFeed";
import "./styles.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <MarketFeedProvider>
      <App />
    </MarketFeedProvider>
  </StrictMode>,
);
