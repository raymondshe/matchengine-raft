"use strict";

const STREAMS = [
  "bookTicker", "trade", "sweep", "depth@100ms", "depth@1000ms",
  "depth5@100ms", "depth10@100ms", "depth20@1000ms",
  "kline_1s", "kline_1m", "status", "lifecycle",
];
const DEFAULT_STREAMS = ["bookTicker", "depth@100ms", "trade", "kline_1s"];
const MAX_FRAMES = 120;
const MAX_TRADES = 40;
const DEPTH_ROWS = 12;

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
  book: { bids: new Map(), asks: new Map(), lastUpdateId: null, stale: false },
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
}

async function loadInfo() {
  try {
    const info = await (await fetch("/info")).json();
    state.symbols = Array.isArray(info.symbols) ? info.symbols : [];
    if (info.lifecycle === false) {
      const index = STREAMS.indexOf("lifecycle");
      if (index >= 0) STREAMS.splice(index, 1);
      state.selected.delete("lifecycle");
    }
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
  state.book = { bids: new Map(), asks: new Map(), lastUpdateId: null, stale: false };
  state.trades = [];
  state.frames = [];
  state.frameCount = 0;
  $("frame-count").textContent = "0 frames";
  $("frame-kind").textContent = "—";
  renderFrames();
  trimTrades();
  renderBook();
  $("book-source").textContent = "no data";
  $("book-source").className = "badge neutral";
  setBookNote(`Waiting for ${displaySymbol()} depth.`, false);
  $("kline-interval").textContent = "—";
  $("kline").textContent = "—";
  $("status").textContent = "—";
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
    case "depthSnapshot": applySnapshot(data, "snapshot"); break;
    case "partialDepth": applySnapshot(data, "partial"); break;
    case "depthUpdate": applyUpdate(data); break;
    case "trade": addTrade(data); break;
    case "sweep": addSweep(data); break;
    case "kline": renderKline(data); break;
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
  const bid = Number(data.b), ask = Number(data.a);
  const hasBid = Number.isFinite(bid) && bid > 0;
  const hasAsk = Number.isFinite(ask) && ask > 0;
  $("best-bid").textContent = hasBid ? data.b : "—";
  $("best-bid-size").textContent = hasBid && data.B ? `size ${data.B}` : "—";
  $("best-ask").textContent = hasAsk ? data.a : "—";
  $("best-ask-size").textContent = hasAsk && data.A ? `size ${data.A}` : "—";
  $("spread").textContent = hasBid && hasAsk ? (ask - bid).toFixed(6) : "—";
  $("mid").textContent = hasBid && hasAsk ? `mid ${((ask + bid) / 2).toFixed(6)}` : "—";
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
  renderLevels($("bids-body"), bids);
  renderLevels($("asks-body"), asks);
  const bestBid = bids[0], bestAsk = asks[0];
  $("best-bid").textContent = bestBid ? bestBid[0] : "—";
  $("best-bid-size").textContent = bestBid ? `size ${bestBid[1]}` : "—";
  $("best-ask").textContent = bestAsk ? bestAsk[0] : "—";
  $("best-ask-size").textContent = bestAsk ? `size ${bestAsk[1]}` : "—";
  $("spread").textContent = bestBid && bestAsk
    ? (Number(bestAsk[0]) - Number(bestBid[0])).toFixed(6)
    : "—";
  $("mid").textContent = bestBid && bestAsk
    ? `mid ${((Number(bestAsk[0]) + Number(bestBid[0])) / 2).toFixed(6)}`
    : "—";
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
  for (const [price, size] of levels) {
    const row = document.createElement("tr");
    for (const value of [price, size]) {
      const cell = document.createElement("td");
      cell.className = "num"; cell.textContent = value;
      row.appendChild(cell);
    }
    body.appendChild(row);
  }
}

function addTrade(data) {
  state.trades.unshift({
    at: new Date(data.T || data.E || Date.now()).toLocaleTimeString(),
    side: data.m ? "SELL" : "BUY",
    price: data.p,
    qty: data.q,
    fee: data.tf ? `${data.tf} ${data.tfa || ""}` : "—",
  });
  trimTrades();
}

function addSweep(data) {
  state.trades.unshift({
    at: new Date(data.T || data.E || Date.now()).toLocaleTimeString(),
    side: data.m ? "SELL" : "BUY",
    price: `${data.volume} @ ${data.fills?.[0]?.p || "—"}`,
    qty: data.quoteVolume,
    fee: "sweep",
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
    cell.colSpan = 5; cell.className = "note"; cell.textContent = "no trades yet";
    row.appendChild(cell); body.appendChild(row);
    return;
  }
  for (const trade of state.trades) {
    const row = document.createElement("tr");
    const side = document.createElement("td");
    side.textContent = trade.side;
    side.className = trade.side === "BUY" ? "buy" : "sell";
    row.appendChild(cellText(trade.at));
    row.appendChild(side);
    row.appendChild(cellText(trade.price, "num"));
    row.appendChild(cellText(trade.qty, "num"));
    row.appendChild(cellText(trade.fee, "num"));
    body.appendChild(row);
  }
}

function cellText(value, className) {
  const cell = document.createElement("td");
  if (className) cell.className = className;
  cell.textContent = value === undefined || value === null ? "—" : String(value);
  return cell;
}

function renderKline(data) {
  const k = data.k || {};
  $("kline-interval").textContent = k.i || "—";
  const container = $("kline");
  container.replaceChildren();
  const fields = [
    ["Open", k.o], ["High", k.h], ["Low", k.l], ["Close", k.c],
    ["Volume", k.v], ["Quote", k.q], ["Trades", k.n], ["Closed", k.x ? "yes" : "no"],
  ];
  for (const [label, value] of fields) {
    const box = document.createElement("div");
    const name = document.createElement("span");
    name.textContent = label;
    const strong = document.createElement("strong");
    strong.textContent = value === undefined || value === null ? "—" : String(value);
    box.append(name, strong);
    container.appendChild(box);
  }
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
