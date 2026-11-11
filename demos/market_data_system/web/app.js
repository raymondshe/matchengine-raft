"use strict";

const STREAMS = [
  "bookTicker", "trade", "sweep", "depth@100ms", "depth@1000ms",
  "depth5@100ms", "depth10@100ms", "depth20@1000ms",
  "kline_1s", "kline_1m", "status", "lifecycle",
];
const DEFAULT_STREAMS = ["bookTicker", "depth@100ms", "trade", "kline_1s", "status"];
const MAX_FRAMES = 120;
const MAX_TRADES = 40;
const DEPTH_ROWS = 10;

const $ = (id) => document.getElementById(id);

const state = {
  symbols: [],
  symbol: null,
  ws: null,
  connected: false,
  // The symbol whose stream names are currently subscribed on the socket.
  subscribedSymbol: null,
  selected: new Set(DEFAULT_STREAMS),
  frames: [],
  trades: [],
  book: {
    bids: new Map(),
    asks: new Map(),
    lastUpdateId: null,
    stale: false,
    stream: null,
    streamKind: null,
  },
  // Last non-empty top of book. A frame that reports an empty side leaves the
  // previous value in place, and spread/mid are computed from the retained
  // bid and ask.
  bbo: { bid: null, bidSize: null, ask: null, askSize: null },
  // One candlestick chart follows one kline stream; a second interval stream
  // is ignored until the user toggles the kline subscriptions.
  kline: {
    chart: null,
    candles: null,
    volume: null,
    stream: null,
    interval: null,
    lastTime: null,
  },
  frameCount: 0,
  metricsTimer: null,
};

function wsBase() {
  const protocol = location.protocol === "https:" ? "wss:" : "ws:";
  return `${protocol}//${location.host}`;
}

function symbolSpec(name = state.symbol) {
  return state.symbols.find((spec) => spec.symbol === name) || null;
}

function displaySymbol(name = state.symbol) {
  return name ? name.toUpperCase() : "—";
}

function streamName(stream, symbol = state.symbol) {
  return `${symbol}@${stream}`;
}

function setHint(text, warn = false) {
  const el = $("hint");
  el.textContent = text || "";
  el.className = `hint${warn ? " warn" : ""}`;
}

function renderStreams() {
  const container = $("streams");
  container.replaceChildren();
  for (const stream of STREAMS) {
    const label = document.createElement("label");
    const input = document.createElement("input");
    input.type = "checkbox";
    input.checked = state.selected.has(stream);
    input.dataset.stream = stream;
    input.addEventListener("change", () => toggleStream(stream, input.checked));
    label.appendChild(input);
    label.appendChild(document.createTextNode(stream));
    container.appendChild(label);
  }
}

function toggleStream(stream, checked) {
  if (checked) state.selected.add(stream); else state.selected.delete(stream);
  // Each depth stream has its own update-id sequence; switch the book to a
  // fresh source so frames from the previous stream cannot clobber it.
  if (stream.startsWith("depth")) resetBookSource();
  // The chart shows one interval at a time; toggling kline streams resets it
  // so the newly selected stream can claim the series.
  if (stream.startsWith("kline")) resetKlineChart();
  if (!state.ws || !state.connected || state.subscribedSymbol !== state.symbol) return;
  const method = checked ? "SUBSCRIBE" : "UNSUBSCRIBE";
  sendControl(method, [streamName(stream)]);
}

let controlId = 0;
function nextId() { controlId += 1; return controlId; }

function sendControl(method, params) {
  if (!state.ws || !state.connected) return null;
  const id = nextId();
  state.ws.send(JSON.stringify({ method, params, id }));
  return id;
}

function sendList() {
  if (!state.ws || !state.connected) return null;
  const id = nextId();
  state.ws.send(JSON.stringify({ method: "LIST_SUBSCRIPTIONS", id }));
  return id;
}

function defaultUrl() {
  return `${wsBase()}/ws`;
}

function renderSymbolOptions() {
  const select = $("symbol");
  select.replaceChildren();
  for (const spec of state.symbols) {
    const option = document.createElement("option");
    option.value = spec.symbol;
    option.textContent = `${spec.symbol.toUpperCase()} · ${spec.base_asset}/${spec.quote_asset} · id ${spec.symbol_id}`;
    select.appendChild(option);
  }
  if (state.symbol) select.value = state.symbol;
  select.disabled = state.symbols.length === 0;
}

function updateSymbolChip() {
  const spec = symbolSpec();
  $("symbol-chip").textContent = spec
    ? `Symbol ${displaySymbol()} · id ${spec.symbol_id}`
    : "Symbol —";
  $("market-pair").textContent = displaySymbol();
  $("market-pair-sub").textContent = spec
    ? `${spec.base_asset} / ${spec.quote_asset} · id ${spec.symbol_id}`
    : "—";
  $("book-price-label").textContent = spec ? `Price (${spec.quote_asset})` : "Price";
  $("book-size-label").textContent = spec ? `Size (${spec.base_asset})` : "Size";
}

async function loadInfo() {
  try {
    const info = await (await fetch("/info")).json();
    state.symbols = Array.isArray(info.symbols) ? info.symbols : [];
    // Honor the server's advertised catalogue: a public deployment exposes
    // only the Binance-compatible streams, while the demo enables extensions.
    const hasCatalogue = Array.isArray(info.streams) && info.streams.length > 0;
    let advertiseStatus = true;
    if (hasCatalogue) {
      const advertised = new Set(info.streams);
      advertiseStatus = advertised.has("status");
      for (let index = STREAMS.length - 1; index >= 0; index -= 1) {
        const stream = STREAMS[index];
        if (!advertised.has(stream)) {
          STREAMS.splice(index, 1);
          state.selected.delete(stream);
        }
      }
      if (advertiseStatus) state.selected.add("status");
    }
    $("status-panel").hidden = !advertiseStatus;
    document.querySelector(".diag-grid").classList.toggle("single", !advertiseStatus);
    if (state.symbols.length && !state.symbol) state.symbol = state.symbols[0].symbol;
    renderSymbolOptions();
    updateSymbolChip();
    renderStreams();
    resetSymbolView();
    if (state.symbols.length) {
      setHint(`${state.symbols.length} symbol(s) loaded. Connect, then switch from the dropdown.`);
    } else {
      setHint("No symbols configured in /info.", true);
    }
  } catch (_) {
    $("symbol-chip").textContent = "Symbol —";
    setHint("Could not load /info.", true);
  }
  $("url").value = defaultUrl();
}

function resetSymbolView() {
  resetBookSource();
  resetBbo();
  state.trades = [];
  state.frames = [];
  state.frameCount = 0;
  $("frame-count").textContent = "0 frames";
  $("frame-kind").textContent = "—";
  renderFrames();
  trimTrades();
  resetKlineChart();
  $("status").textContent = "waiting for status frames…";
}

function resetBookSource() {
  state.book = {
    bids: new Map(),
    asks: new Map(),
    lastUpdateId: null,
    stale: false,
    stream: null,
    streamKind: null,
  };
  renderBook();
  $("book-source").textContent = "no data";
  $("book-source").className = "badge neutral";
  setBookNote(`Waiting for ${displaySymbol()} depth.`, false);
}

// One visual book follows exactly one depth stream. Mixing streams resets
// `lastUpdateId` from another stream's sequence, which drops or gaps the
// stream that was actually feeding the book.
function claimBook(kind, raw) {
  const partial = kind === "partialDepth";
  // Single-stream connections have no wrapper key; a diff snapshot and its
  // updates still belong to the same stream.
  const key = raw && raw.stream ? raw.stream : (partial ? "partial" : "diff");
  if (state.book.stream === key) return true;
  if (state.book.stream === null) {
    state.book.stream = key;
    state.book.streamKind = partial ? "partial" : "diff";
    return true;
  }
  // A diff stream is authoritative: its subscribe-time snapshot replaces the
  // partial book, so depth@100ms keeps the book live when a partial stream is
  // also selected.
  if (!partial && kind === "depthSnapshot" && state.book.streamKind === "partial") {
    state.book.stream = key;
    state.book.streamKind = "diff";
    return true;
  }
  return false;
}

function setBookNote(text, warn) {
  $("book-note").textContent = text;
  $("book-note").className = `note${warn ? " warn" : ""}`;
}

function setSymbol(name) {
  if (!name || name === state.symbol) return;
  const previous = state.subscribedSymbol;
  state.symbol = name;
  $("symbol").value = name;
  updateSymbolChip();
  resetSymbolView();
  refreshService();

  if (!state.ws || !state.connected) {
    setHint(`Showing ${displaySymbol()}. Connect to subscribe.`);
    return;
  }

  const streams = [...state.selected];
  if (previous && previous !== name) {
    sendControl("UNSUBSCRIBE", streams.map((stream) => streamName(stream, previous)));
  }
  if (streams.length) {
    sendControl("SUBSCRIBE", streams.map((stream) => streamName(stream, name)));
  }
  state.subscribedSymbol = name;
  setHint(`Switched to ${displaySymbol()} on the same connection (unsubscribed ${displaySymbol(previous)}, subscribed ${displaySymbol(name)}).`);
}

function connect() {
  if (!state.symbol) {
    setHint("Symbols are still loading; try again in a moment.", true);
    return;
  }
  if (state.ws) disconnect();
  const url = $("url").value.trim() || defaultUrl();
  let ws;
  try {
    ws = new WebSocket(url);
  } catch (error) {
    setHint(`Invalid URL: ${error.message}`, true);
    return;
  }
  state.ws = ws;
  setConnection("connecting", "warn");
  $("connect").disabled = true;
  $("disconnect").disabled = false;
  ws.addEventListener("open", () => {
    state.connected = true;
    setConnection("connected", "good");
    const streams = [...state.selected];
    if (streams.length) {
      sendControl("SUBSCRIBE", streams.map((stream) => streamName(stream)));
    }
    state.subscribedSymbol = state.symbol;
    setHint(`Connected; subscribed ${streams.length} stream(s) for ${displaySymbol()}.`);
  });
  ws.addEventListener("message", (event) => handleFrame(event.data));
  ws.addEventListener("close", (event) => {
    state.connected = false;
    state.subscribedSymbol = null;
    state.ws = null;
    setConnection(`closed (${event.code})`, "bad");
    $("connect").disabled = false;
    $("disconnect").disabled = true;
    setHint(event.reason || "Connection closed; reconnect to resume.", true);
  });
  ws.addEventListener("error", () => setHint("WebSocket error; see the browser console.", true));
}

function disconnect() {
  if (!state.ws) return;
  try { state.ws.close(1000, "client disconnect"); } catch (_) {}
  state.ws = null;
  state.connected = false;
  state.subscribedSymbol = null;
  setConnection("disconnected", "");
  $("connect").disabled = false;
  $("disconnect").disabled = true;
}

function setConnection(label, tone) {
  $("conn-label").textContent = label;
  $("conn-chip").className = `chip${tone ? ` ${tone}` : ""}`;
  $("conn-dot").className = `dot${tone ? ` ${tone}` : ""}`;
}

function handleFrame(text) {
  let raw;
  try { raw = JSON.parse(text); } catch (_) { return; }
  const data = raw && raw.data ? raw.data : raw;
  // Frames already queued for the previous symbol are dropped, not rendered.
  if (data && data.s && state.symbol && data.s !== state.symbol) return;

  state.frameCount += 1;
  $("frame-count").textContent = `${state.frameCount} frames`;
  const kind = kindOf(data);
  $("frame-kind").textContent = kind;
  state.frames.push({ kind, stream: raw && raw.stream, at: new Date().toLocaleTimeString(), json: data });
  if (state.frames.length > MAX_FRAMES) state.frames.splice(0, state.frames.length - MAX_FRAMES);
  renderFrames();

  switch (kind) {
    case "bookTicker": renderBookTicker(data); break;
    case "depthSnapshot":
      if (claimBook(kind, raw)) applySnapshot(data, "snapshot");
      break;
    case "partialDepth":
      if (claimBook(kind, raw)) applySnapshot(data, "partial");
      break;
    case "depthUpdate":
      if (claimBook(kind, raw)) applyUpdate(data);
      break;
    case "trade": addTrade(data); break;
    case "sweep": addSweep(data); break;
    case "kline": renderKline(data, raw); break;
    case "status": $("status").textContent = JSON.stringify(data, null, 2); break;
    case "resync":
      state.book.stale = true;
      setBookNote(`Resync requested (${data.reason || "gap"}); waiting for a fresh depth snapshot.`, true);
      break;
    case "result":
      if (Array.isArray(data.result)) {
        const list = data.result.length ? data.result.join(", ") : "(none)";
        setHint(`Subscriptions: ${list}`);
      } else {
        setHint(`Control result (id ${data.id}) for ${displaySymbol()}.`);
      }
      break;
    case "error": setHint(`Server error ${data.error?.code}: ${data.error?.msg}`, true); break;
  }
}

function kindOf(data) {
  if (!data || typeof data !== "object") return "unknown";
  if (data.e) return data.e;
  if (data.error) return "error";
  if (data.result !== undefined) return "result";
  if (data.bids && data.asks) return "partialDepth";
  if (data.b !== undefined && data.a !== undefined && data.u !== undefined) return "bookTicker";
  return "unknown";
}

function renderBookTicker(data) {
  updateBbo(data.b, data.B, data.a, data.A);
}

function resetBbo() {
  state.bbo = { bid: null, bidSize: null, ask: null, askSize: null };
  renderBbo();
}

// Keep the last complete top of book. A frame that reports an empty side is
// not a new two-sided quote: replacing one side with it would pair a fresh
// price with a stale one and can show a crossed spread.
function updateBbo(bid, bidSize, ask, askSize) {
  if (!(Number(bid) > 0) || !(Number(ask) > 0)) return;
  const bbo = state.bbo;
  bbo.bid = bid;
  bbo.ask = ask;
  if (bidSize !== undefined && bidSize !== null && bidSize !== "") bbo.bidSize = bidSize;
  if (askSize !== undefined && askSize !== null && askSize !== "") bbo.askSize = askSize;
  renderBbo();
}

function renderBbo() {
  const { bid, bidSize, ask, askSize } = state.bbo;
  const hasBid = bid !== null;
  const hasAsk = ask !== null;
  const spread = hasBid && hasAsk ? Number(ask) - Number(bid) : null;
  const mid = hasBid && hasAsk ? (Number(ask) + Number(bid)) / 2 : null;
  $("best-bid").textContent = hasBid ? bid : "—";
  $("best-bid-size").textContent = hasBid && bidSize !== null ? `size ${bidSize}` : "—";
  $("best-ask").textContent = hasAsk ? ask : "—";
  $("best-ask-size").textContent = hasAsk && askSize !== null ? `size ${askSize}` : "—";
  $("spread").textContent = spread === null ? "—" : trimNumber(spread);
  $("spread-bps").textContent = spread === null || !mid
    ? "—"
    : `${(spread / mid * 10000).toFixed(1)} bps`;
  $("mid").textContent = mid === null ? "—" : trimNumber(mid);
  $("book-spread").textContent = spread === null ? "—" : trimNumber(spread);
}

// Keep trailing zeros off derived values so 98.5 does not read as 98.500000.
function trimNumber(value) {
  if (!Number.isFinite(value)) return "—";
  return value.toFixed(6).replace(/(\.\d*?)0+$/, "$1").replace(/\.$/, "");
}

function applySnapshot(data, source) {
  state.book.bids.clear();
  state.book.asks.clear();
  for (const [price, size] of data.bids || []) state.book.bids.set(price, size);
  for (const [price, size] of data.asks || []) state.book.asks.set(price, size);
  state.book.lastUpdateId = data.lastUpdateId ?? null;
  state.book.stale = false;
  $("book-source").textContent = source;
  $("book-source").className = "badge good";
  setBookNote(`lastUpdateId ${state.book.lastUpdateId ?? "—"}; updates must be contiguous from here.`, false);
  renderBook();
}

function applyUpdate(data) {
  const book = state.book;
  if (book.stale) return;
  if (book.lastUpdateId === null) {
    setBookNote("Waiting for a depth snapshot before applying updates.", true);
    return;
  }
  if (Number(data.u) <= book.lastUpdateId) return;
  const first = Number(data.U);
  const last = Number(data.u);
  if (first <= book.lastUpdateId + 1 && last >= book.lastUpdateId + 1) {
    applyLevels(data.b, book.bids);
    applyLevels(data.a, book.asks);
    book.lastUpdateId = last;
    setBookNote(`lastUpdateId ${last} · ${(data.b || []).length + (data.a || []).length} level change(s)`, false);
    renderBook();
  } else {
    book.stale = true;
    setBookNote(`Gap: expected U <= ${book.lastUpdateId + 1}, got ${first}. Waiting for a resync snapshot.`, true);
  }
}

function applyLevels(levels, map) {
  for (const [price, size] of levels || []) {
    if (Number(size) === 0) map.delete(price); else map.set(price, size);
  }
}

function renderBook() {
  const bids = [...state.book.bids.entries()].sort((a, b) => Number(b[0]) - Number(a[0])).slice(0, DEPTH_ROWS);
  const asks = [...state.book.asks.entries()].sort((a, b) => Number(a[0]) - Number(b[0])).slice(0, DEPTH_ROWS);
  // Ask rows run highest-to-lowest so the best ask sits just above the spread
  // row, mirroring a trading-terminal ladder.
  renderLevels($("bids-body"), bids);
  renderLevels($("asks-body"), asks.slice().reverse());
  const bestBid = bids[0], bestAsk = asks[0];
  updateBbo(
    bestBid ? bestBid[0] : null,
    bestBid ? bestBid[1] : null,
    bestAsk ? bestAsk[0] : null,
    bestAsk ? bestAsk[1] : null,
  );
}

function renderLevels(body, levels) {
  body.replaceChildren();
  if (!levels.length) {
    const row = document.createElement("tr");
    const cell = document.createElement("td");
    cell.colSpan = 2; cell.className = "note"; cell.textContent = "no levels";
    row.appendChild(cell); body.appendChild(row);
    return;
  }
  // Scale the subtle depth bar against the largest visible level on this side.
  const max = levels.reduce((value, level) => Math.max(value, Number(level[1]) || 0), 0);
  for (const [price, size] of levels) {
    const row = document.createElement("tr");
    const share = max > 0 ? Math.min(100, (Number(size) / max) * 100) : 0;
    row.style.setProperty("--depth", `${share.toFixed(1)}%`);
    for (const value of [price, size]) {
      const cell = document.createElement("td");
      cell.className = "num";
      cell.textContent = value;
      row.appendChild(cell);
    }
    body.appendChild(row);
  }
}

// The tape column is tight; a 24-hour HH:MM:SS clock keeps every row on one
// line while the full timestamp stays available as a hover title.
function clockStamp(ms) {
  const date = new Date(ms);
  return {
    time: date.toLocaleTimeString([], {
      hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false,
    }),
    title: date.toLocaleString(),
  };
}

function addTrade(data) {
  const stamp = clockStamp(data.T || data.E || Date.now());
  state.trades.unshift({
    at: stamp.time,
    atTitle: stamp.title,
    side: data.m ? "SELL" : "BUY",
    price: data.p,
    qty: data.q,
  });
  trimTrades();
}

function addSweep(data) {
  const stamp = clockStamp(data.T || data.E || Date.now());
  state.trades.unshift({
    at: stamp.time,
    atTitle: stamp.title,
    side: data.m ? "SELL" : "BUY",
    price: `${data.volume} @ ${data.fills?.[0]?.p || "—"}`,
    qty: data.quoteVolume,
  });
  trimTrades();
}

function trimTrades() {
  if (state.trades.length > MAX_TRADES) state.trades.length = MAX_TRADES;
  const body = $("trades-body");
  body.replaceChildren();
  $("trade-count").textContent = String(state.trades.length);
  if (!state.trades.length) {
    const row = document.createElement("tr");
    const cell = document.createElement("td");
    cell.colSpan = 4; cell.className = "note"; cell.textContent = "no trades yet";
    row.appendChild(cell); body.appendChild(row);
    return;
  }
  for (const trade of state.trades) {
    const row = document.createElement("tr");
    const time = cellText(trade.at);
    if (trade.atTitle) time.title = trade.atTitle;
    const side = document.createElement("td");
    side.textContent = trade.side;
    side.className = trade.side === "BUY" ? "buy" : "sell";
    row.appendChild(time);
    row.appendChild(side);
    row.appendChild(cellText(trade.price, "num"));
    row.appendChild(cellText(trade.qty, "num"));
    body.appendChild(row);
  }
}

function cellText(value, className) {
  const cell = document.createElement("td");
  if (className) cell.className = className;
  cell.textContent = value === undefined || value === null ? "—" : String(value);
  return cell;
}

function show(value) {
  return value === undefined || value === null ? "—" : String(value);
}

// Lightweight Charts treats timestamps as UTC; the rest of the page uses the
// browser's local clock, so format both tick marks and the crosshair label in
// local time to keep the chart aligned with the trade tape.
function localTime(seconds, withSeconds = true) {
  if (typeof seconds !== "number") return "";
  return new Date(seconds * 1000).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    ...(withSeconds ? { second: "2-digit" } : {}),
    hour12: false,
  });
}

function ensureKlineChart() {
  if (state.kline.chart) return true;
  const lib = window.LightweightCharts;
  const container = $("kline-chart");
  if (!lib || !container) {
    $("kline-stats").textContent = "Chart library unavailable; kline frames are still shown under Frames.";
    return false;
  }
  const chart = lib.createChart(container, {
    autoSize: true,
    layout: {
      background: { type: lib.ColorType.Solid, color: "transparent" },
      textColor: "#9fb2ce",
      // Apache-2.0 attribution; the upstream banner is kept in the vendored file.
      attributionLogo: true,
    },
    grid: {
      vertLines: { color: "rgba(36, 51, 79, 0.55)" },
      horzLines: { color: "rgba(36, 51, 79, 0.55)" },
    },
    localization: {
      timeFormatter: (time) => localTime(time),
    },
    rightPriceScale: { borderColor: "#24334f" },
    timeScale: {
      borderColor: "#24334f",
      timeVisible: true,
      secondsVisible: true,
      rightOffset: 2,
      tickMarkFormatter: (time) => localTime(time, state.kline.interval !== "1m"),
    },
  });
  const candles = chart.addSeries(lib.CandlestickSeries, {
    upColor: "#4ade80",
    downColor: "#f87171",
    borderUpColor: "#4ade80",
    borderDownColor: "#f87171",
    wickUpColor: "#4ade80",
    wickDownColor: "#f87171",
  });
  const volume = chart.addSeries(lib.HistogramSeries, {
    priceFormat: { type: "volume" },
    priceScaleId: "",
    priceLineVisible: false,
    lastValueVisible: false,
  });
  // Keep volume in the lower fifth of the pane without a second price scale.
  volume.priceScale().applyOptions({ scaleMargins: { top: 0.78, bottom: 0 } });
  state.kline.chart = chart;
  state.kline.candles = candles;
  state.kline.volume = volume;
  return true;
}

// One chart follows one kline stream, like the depth book. If both
// kline_1s and kline_1m are selected, the first to produce a frame owns the
// chart; toggling a kline stream clears the claim so the other can take over.
function claimKline(raw) {
  const key = raw && raw.stream ? raw.stream : "kline";
  if (state.kline.stream === key) return true;
  if (state.kline.stream === null) {
    state.kline.stream = key;
    return true;
  }
  return false;
}

function resetKlineChart() {
  state.kline.stream = null;
  state.kline.interval = null;
  state.kline.lastTime = null;
  if (state.kline.candles) state.kline.candles.setData([]);
  if (state.kline.volume) state.kline.volume.setData([]);
  $("kline-interval").textContent = "—";
  $("kline-stats").textContent = "Subscribe to kline_1s or kline_1m.";
}

function renderKline(data, raw) {
  const k = data.k || {};
  if (!claimKline(raw) || !ensureKlineChart()) return;
  const interval = k.i || "—";
  if (state.kline.interval !== interval) {
    // A different interval means different bar times; keeping the old series
    // would make Lightweight Charts reject the update as out of order.
    state.kline.interval = interval;
    state.kline.lastTime = null;
    state.kline.candles.setData([]);
    state.kline.volume.setData([]);
  }
  $("kline-interval").textContent = interval;

  const time = Math.floor(Number(k.t) / 1000);
  const open = Number(k.o);
  const high = Number(k.h);
  const low = Number(k.l);
  const close = Number(k.c);
  if ([time, open, high, low, close].every(Number.isFinite)) {
    state.kline.candles.update({ time, open, high, low, close });
    const volume = Number(k.v);
    if (Number.isFinite(volume)) {
      state.kline.volume.update({
        time,
        value: volume,
        color: close >= open ? "rgba(74, 222, 128, 0.45)" : "rgba(248, 113, 113, 0.45)",
      });
    }
    if (state.kline.lastTime !== time) {
      state.kline.lastTime = time;
      state.kline.chart.timeScale().scrollToRealTime();
    }
  }
  $("kline-stats").textContent = [
    `O ${show(k.o)}`, `H ${show(k.h)}`, `L ${show(k.l)}`, `C ${show(k.c)}`,
    `V ${show(k.v)}`, `${show(k.n)} trades`, k.x ? "closed" : "open",
  ].join(" · ");
}

function renderFrames() {
  const container = $("frames");
  if (!state.frames.length) { container.textContent = "Connect to see frames."; return; }
  container.textContent = state.frames.slice().reverse().map((frame) => {
    const prefix = frame.stream ? `[${frame.at}] ${frame.stream} ` : `[${frame.at}] `;
    return `${prefix}${JSON.stringify(frame.json)}`;
  }).join("\n");
}

async function refreshService() {
  try {
    const [metricsResponse, readyResponse] = await Promise.all([fetch("/metrics"), fetch("/ready")]);
    const metrics = await metricsResponse.json();
    const ready = await readyResponse.json();
    $("metrics").textContent = JSON.stringify(metrics, null, 2);
    const current = ready.symbols ? ready.symbols[state.symbol] : null;
    const isReady = current ? current.ready === true : ready.ready === true;
    $("ready-chip").textContent = `Ready ${displaySymbol()} ${isReady ? "yes" : "no"}`;
    $("ready-chip").className = `chip ${isReady ? "good" : "bad"}`;
    const entries = Object.entries(ready.symbols || {});
    $("ready-note").textContent = entries.length
      ? entries.map(([name, entry]) => {
        const snapshot = entry.snapshot_seen ? "snapshot yes" : "snapshot no";
        const depth = entry.depth_stale ? "depth stale" : "depth fresh";
        return `${name.toUpperCase()}: ${entry.ready ? "ready" : "not ready"} (${snapshot}, ${depth})`;
      }).join(" · ")
      : "—";
    $("metrics-age").textContent = `updated ${new Date().toLocaleTimeString()}`;
  } catch (_) {
    $("metrics").textContent = "metrics unavailable";
  }
}

function clearAll() {
  state.frames = [];
  state.trades = [];
  state.frameCount = 0;
  $("frame-count").textContent = "0 frames";
  $("frame-kind").textContent = "—";
  renderFrames();
  trimTrades();
}

function bind() {
  $("symbol").addEventListener("change", (event) => setSymbol(event.target.value));
  $("connect").addEventListener("click", connect);
  $("disconnect").addEventListener("click", disconnect);
  $("list").addEventListener("click", () => {
    if (state.ws && state.connected) sendList();
  });
  $("clear").addEventListener("click", clearAll);
  loadInfo();
  refreshService();
  state.metricsTimer = setInterval(refreshService, 2000);
}

bind();
