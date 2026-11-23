import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Dev proxy mirrors the nginx routes the container uses, so `npm run dev`
// talks to the live OrbStack fleet unchanged:
//   /md/*        -> global market-data service (WS /ws, /stream, /info, /ready)
//   /demo/api/*  -> order-system BFF
export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      "/md/ws": {
        target: "http://ws-fx-eu-02.match-eu-1.example",
        ws: true,
        rewrite: (path) => path.replace(/^\/md/, ""),
      },
      "/md/stream": {
        target: "http://ws-fx-eu-02.match-eu-1.example",
        ws: true,
        rewrite: (path) => path.replace(/^\/md/, ""),
      },
      "/md": {
        target: "http://ws-fx-eu-02.match-eu-1.example",
        rewrite: (path) => path.replace(/^\/md/, ""),
      },
      "/demo/api": {
        target: "http://orders.match-eu-1.example",
      },
    },
  },
});
