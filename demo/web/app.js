const state = {
  node: null,
  symbol: null,
  book: null,
  orders: [],
  stops: [],
  events: new Map(),
  cursor: null,
  paused: false,
  side: "Buy",
  stateLoading: false,
  eventsLoading: false,
  fullBook: null,
  fullPage: 0,
  pageSize: 50,
  selectedEvent: null,
  symbolFormReady: false,
  stateTimer: null,
  eventTimer: null,
};

const $ = (id) => document.getElementById(id);

function setText(id, value) {
  const element = $(id);
  if (element) element.textContent = value === null || value === undefined || value === "" ? "—" : String(value);
}

function setChip(id, label, tone) {
  const element = $(id);
  if (!element) return;
  element.className = `chip${tone ? ` ${tone}` : ""}`;
  element.replaceChildren();
  const dot = document.createElement("span");
  dot.className = `dot${tone ? ` ${tone}` : ""}`;
  element.appendChild(dot);
  element.appendChild(document.createTextNode(label));
}

function clear(element) {
  if (element) element.replaceChildren();
}

function cell(row, value, className) {
  const td = document.createElement("td");
  if (className) td.className = className;
  td.textContent = value === null || value === undefined || value === "" ? "—" : String(value);
  row.appendChild(td);
  return td;
}

function badge(text, tone) {
  const span = document.createElement("span");
  span.className = `badge ${tone || "neutral"}`;
  span.textContent = text;
  return span;
}

function rowWithBadge(row, text, tone) {
  const td = document.createElement("td");
  td.appendChild(badge(text, tone));
  row.appendChild(td);
}

function emptyRow(body, columns, text) {
  clear(body);
  const row = document.createElement("tr");
  const td = document.createElement("td");
  td.colSpan = columns;
  td.className = "empty";
  td.textContent = text;
  row.appendChild(td);
  body.appendChild(row);
}

function sideClass(side) {
  return side === "Sell" ? "side-sell" : "side-buy";
}

function tifLabel(tif) {
  if (typeof tif === "string") return tif;
  if (tif && typeof tif === "object") {
    const entry = Object.entries(tif)[0];
    if (entry) {
      const [kind, value] = entry;
      if (kind.toLowerCase() === "gtd" && Number.isFinite(Number(value))) {
        return `GTD ${new Date(Number(value)).toLocaleString()}`;
      }
      return `${kind} ${value}`;
    }
  }
  return "—";
}

function eventKey(id) {
  return `${id.log_index}:${id.ordinal}`;
}

function eventLabel(id) {
  return id ? `${id.log_index}:${id.ordinal}` : "—";
}

function compareId(a, b) {
  if (!a || !b) return 0;
  return a.log_index - b.log_index || a.ordinal - b.ordinal;
}

function sortedEvents() {
  return [...state.events.values()].sort((a, b) => compareId(a.event_id, b.event_id));
}

function showWarning(title, text) {
  $("warning-banner").hidden = false;
  setText("warning-title", title);
  setText("warning-text", text);
}

function hideWarning() {
  $("warning-banner").hidden = true;
}

async function fetchJSON(url, options = {}) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), options.timeout || 5000);
  try {
    const response = await fetch(url, {
      ...options,
      signal: controller.signal,
      headers: { "Content-Type": "application/json", ...(options.headers || {}) },
    });
    const body = await response.json().catch(() => null);
    if (!response.ok) {
      const message = body?.error?.message || `${response.status} ${response.statusText}`;
      const error = new Error(message);
      error.body = body;
      error.status = response.status;
      throw error;
    }
    return body;
  } finally {
    clearTimeout(timer);
  }
}

function postJSON(url, body) {
  return fetchJSON(url, {
    method: "POST",
    headers: { "X-Demo-Action": "submit" },
    body: JSON.stringify(body),
  });
}

function nowLabel() {
  return `updated ${new Date().toLocaleTimeString()}`;
}

function renderNode() {
  const node = state.node;
  if (!node) return;
  setChip("node-chip", `node-${node.id}`, node.healthy ? "good" : "bad");
  setChip("leader-chip", `Leader ${node.leader_id ?? "—"}`, node.leader_id ? "good" : "bad");
  setText("term-chip", `Term ${node.term}`);
  const ready = $("ready-chip");
  ready.className = `chip ${node.ready ? "good" : "bad"}`;
  ready.textContent = node.ready ? "Ready" : "Not ready";
  setText("symbol-chip", state.symbol ? `${state.symbol.config.base_asset.code}/${state.symbol.config.quote_asset.code}` : "Symbol —");
  setText("updated", nowLabel());
  setText("node-engine", node.http_addr);
  setText("node-leader-term", `${node.leader_id ?? "—"} / ${node.term}`);
  setText("node-raft-state", node.raft_state);
  setText("node-book-seq", state.book?.sequence ?? "—");
  $("node-ready-badge").className = `badge ${node.ready ? "good" : "bad"}`;
  $("node-ready-badge").textContent = node.ready ? "Ready" : "Not ready";

  if (!node.healthy) {
    showWarning("Node unhealthy", node.ready ? "Storage or Raft is degraded." : "The node is not ready.");
  } else if (!node.is_leader) {
    showWarning(
      "Follower view",
      node.leader_addr
        ? `Book reads may lag. Writes and events follow leader ${node.leader_id}.`
        : `Book reads may lag. Writes and events follow leader ${node.leader_id ?? "unknown"}.`,
    );
  } else {
    hideWarning();
  }
}

function renderSymbol() {
  const symbol = state.symbol;
  if (!symbol) return;
  const config = symbol.config;
  setText("desk-title", `${config.base_asset.code} / ${config.quote_asset.code}`);
  setText("instrument-subtitle", `${config.base_asset.code} / ${config.quote_asset.code} · ${config.base_asset.class} / ${config.quote_asset.class}`);
  setText("tick-lot", `${config.price_tick} / ${config.base_lot}`);
  setText("config-symbol-id", config.symbol_id);
  setText("config-base", `${config.base_asset.code} · ${config.base_asset.class} · ${config.base_asset.decimals} dp`);
  setText("config-quote", `${config.quote_asset.code} · ${config.quote_asset.class} · ${config.quote_asset.decimals} dp`);
  setText("config-price-decimals", config.price_decimals);
  setText("config-tick", config.price_tick);
  setText("config-lot", config.base_lot);
  setText("config-fee-policy", config.fee_asset_policy);
  setText("config-book-seq", state.book?.sequence ?? "—");
  setText("price-unit", config.quote_asset.code);
  setText("ticket-subtitle", `${$("ticket-type").value} · ${$("ticket-tif").value}`);
  setText("ticket-type-badge", $("ticket-type").value);

  const locked = symbol.locked;
  $("symbol-lock").className = `badge ${locked ? "neutral" : "good"}`;
  $("symbol-lock").textContent = locked ? "locked" : "configurable";
  setText("symbol-banner-title", locked ? "Configuration locked" : "Configuration open");
  setText(
    "symbol-banner-text",
    locked
      ? "Trading started; the symbol and accepted precision can no longer change."
      : "Review the symbol before the first order. It locks after the first accepted order.",
  );

  if (!state.symbolFormReady) {
    $("symbol-base-code").value = config.base_asset.code;
    $("symbol-base-class").value = config.base_asset.class;
    $("symbol-base-decimals").value = config.base_asset.decimals;
    $("symbol-quote-code").value = config.quote_asset.code;
    $("symbol-quote-class").value = config.quote_asset.class;
    $("symbol-quote-decimals").value = config.quote_asset.decimals;
    $("symbol-price-decimals").value = config.price_decimals;
    $("symbol-tick").value = config.price_tick;
    $("symbol-lot").value = config.base_lot;
    $("symbol-policy").value = config.fee_asset_policy;
    state.symbolFormReady = true;
  }
  for (const id of [
    "symbol-base-code", "symbol-base-class", "symbol-base-decimals",
    "symbol-quote-code", "symbol-quote-class", "symbol-quote-decimals",
    "symbol-price-decimals", "symbol-tick", "symbol-lot", "symbol-policy",
    "symbol-submit",
  ]) {
    $(id).disabled = locked;
  }
}

function renderBook() {
  const book = state.book;
  if (!book) return;
  const bids = book.depth.bids || [];
  const asks = book.depth.asks || [];
  setText("best-bid", bids[0]?.price);
  setText("best-bid-foot", bids[0] ? `${bids[0].volume} · ${bids[0].orders} orders` : "no bid");
  setText("best-ask", asks[0]?.price);
  setText("best-ask-foot", asks[0] ? `${asks[0].volume} · ${asks[0].orders} orders` : "no ask");
  setText("spread", book.spread);
  setText("last-trade", book.last_trade_price);
  setText("last-trade-foot", book.last_trade_price ? "last matched price" : "no trade yet");
  setText("book-sequence", book.sequence);
  setText("footer-book", `book ${book.sequence} · last trade ${book.last_trade_price ?? "—"} · spread ${book.spread ?? "—"}`);

  const body = $("depth-body");
  clear(body);
  const rows = [];
  for (const level of [...asks].reverse()) {
    rows.push({ side: "Ask", level });
  }
  for (const level of bids) {
    rows.push({ side: "Bid", level });
  }
  if (!rows.length) {
    emptyRow(body, 4, "No resting levels");
    return;
  }
  let spreadInserted = false;
  for (const { side, level } of rows) {
    if (side === "Bid" && !spreadInserted && asks.length) {
      const spread = document.createElement("tr");
      spread.className = "spread-row";
      const td = document.createElement("td");
      td.colSpan = 4;
      td.textContent = `spread ${book.spread ?? "—"}`;
      spread.appendChild(td);
      body.appendChild(spread);
      spreadInserted = true;
    }
    const row = document.createElement("tr");
    row.className = "level";
    const sideCell = document.createElement("td");
    sideCell.className = `side ${side.toLowerCase()}`;
    sideCell.textContent = side;
    row.appendChild(sideCell);
    cell(row, level.price, "num mono");
    cell(row, level.volume, "num mono");
    cell(row, level.orders, "num");
    row.addEventListener("click", () => {
      $("ticket-price").value = level.price;
    });
    body.appendChild(row);
  }
}

function orderTypeLabel(order) {
  const bits = [order.live];
  const tif = tifLabel(order.tif);
  if (tif !== "GTC") bits.push(tif);
  if (order.post_only) bits.push("post-only");
  if (order.display_volume && order.display_volume !== "0") bits.push(`iceberg ${order.display_volume}`);
  if (order.stop_price && order.stop_price !== "0") bits.push(`stop ${order.stop_price}`);
  return bits.join(" · ");
}

function renderOrders() {
  const body = $("orders-body");
  clear(body);
  const items = state.orders || [];
  setText("orders-count", `${items.length}/${state.stateOrdersTotal ?? items.length}`);
  if (!items.length) {
    emptyRow(body, 6, "No resting orders");
    return;
  }
  for (const order of items) {
    const row = document.createElement("tr");
    cell(row, order.sequence, "mono");
    cell(row, order.side, sideClass(order.side));
    cell(row, order.price, "mono");
    cell(row, order.volume, "mono");
    cell(row, orderTypeLabel(order));
    const actions = document.createElement("td");
    actions.className = "actions";
    const cancel = document.createElement("button");
    cancel.className = "btn ghost";
    cancel.type = "button";
    cancel.textContent = "Cancel";
    cancel.addEventListener("click", () => cancelOrder(order));
    actions.appendChild(cancel);
    if (order.live === "Limit" && (!order.stop_price || order.stop_price === "0")) {
      const amend = document.createElement("button");
      amend.className = "btn";
      amend.type = "button";
      amend.textContent = "Amend";
      amend.addEventListener("click", () => amendOrder(order));
      actions.appendChild(amend);
    }
    row.appendChild(actions);
    body.appendChild(row);
  }
}

function renderStops() {
  const body = $("stops-body");
  clear(body);
  const items = state.stops || [];
  setText("stops-count", items.length);
  if (!items.length) {
    emptyRow(body, 6, "No parked stops");
    return;
  }
  for (const order of items) {
    const row = document.createElement("tr");
    cell(row, order.sequence, "mono");
    cell(row, order.side, sideClass(order.side));
    cell(row, order.stop_price, "mono");
    cell(row, order.volume, "mono");
    cell(row, order.live);
    const actions = document.createElement("td");
    actions.className = "actions";
    const cancel = document.createElement("button");
    cancel.className = "btn ghost";
    cancel.type = "button";
    cancel.textContent = "Cancel";
    cancel.addEventListener("click", () => cancelOrder(order));
    actions.appendChild(cancel);
    row.appendChild(actions);
    body.appendChild(row);
  }
}

function eventRow(event, compact = false) {
  const row = document.createElement("tr");
  cell(row, eventLabel(event.event_id), "event-id");
  rowWithBadge(row, event.kind, event.kind === "fill" ? "good" : event.kind === "rejected" ? "bad" : event.kind === "cancelled" || event.kind === "expired" ? "warn" : "neutral");
  cell(row, event.side, sideClass(event.side));
  if (!compact) {
    cell(row, event.order_sequence, "num mono");
    cell(row, event.counterparty_sequence ?? "—", "num mono");
  }
  cell(row, event.price, "num mono");
  cell(row, event.volume, "num mono");
  if (compact) {
    cell(row, event.order_sequence, "num mono");
  } else {
    cell(row, event.quote_volume, "num mono");
    const fee = document.createElement("td");
    fee.className = "fee";
    if (event.kind === "fill") {
      fee.textContent = `taker ${event.taker_fee} ${event.taker_fee_asset ?? ""} / maker ${event.maker_fee} ${event.maker_fee_asset ?? ""}`;
    } else {
      fee.textContent = "—";
    }
    row.appendChild(fee);
    const reason = document.createElement("td");
    reason.className = event.reason ? "reason mono" : "";
    reason.textContent = event.reason || "";
    row.appendChild(reason);
  }
  return row;
}

function renderRecentEvents() {
  const body = $("recent-events-body");
  clear(body);
  const events = sortedEvents().slice(-3);
  if (!events.length) {
    emptyRow(body, 6, "No events yet");
  } else {
    for (const event of events) body.appendChild(eventRow(event, true));
  }
  setText("recent-status", state.cursor ? `cursor ${eventLabel(state.cursor)} · ${state.events.size} loaded` : "waiting for events");
}

function renderEvents() {
  const body = $("events-body");
  clear(body);
  const kind = $("event-kind").value;
  const side = $("event-side").value;
  const order = $("event-order").value;
  const events = sortedEvents().filter((event) => {
    if (kind && event.kind !== kind) return false;
    if (side && event.side !== side) return false;
    if (order && String(event.order_sequence) !== order && String(event.counterparty_sequence) !== order) return false;
    return true;
  });
  const visible = events.slice(-500);
  if (!visible.length) {
    emptyRow(body, 10, events.length ? "No events match the filter" : "No events in the retained window");
  } else {
    for (const event of visible) {
      const row = eventRow(event);
      row.addEventListener("click", () => {
        state.selectedEvent = event;
        renderEventDetail(event);
      });
      body.appendChild(row);
    }
  }
  setText("events-status", state.cursor ? `cursor ${eventLabel(state.cursor)} · ${state.events.size} loaded · showing ${visible.length}` : "waiting for events");
  setText("footer-events", state.cursor ? `events ${eventLabel(state.cursor)} · ${state.events.size} loaded` : "events —");
  $("events-badge").className = `badge ${state.paused ? "warn" : "good"}`;
  $("events-badge").textContent = state.paused ? "Paused" : "Following";
  $("recent-badge").className = `badge ${state.paused ? "warn" : "good"}`;
  $("recent-badge").textContent = state.paused ? "Paused" : "Following";
  renderOrderFilterOptions();
}

function renderOrderFilterOptions() {
  const select = $("event-order");
  const current = select.value;
  const sequences = new Set();
  for (const event of state.events.values()) {
    sequences.add(event.order_sequence);
    if (event.counterparty_sequence !== null && event.counterparty_sequence !== undefined) {
      sequences.add(event.counterparty_sequence);
    }
  }
  clear(select);
  const all = document.createElement("option");
  all.value = "";
  all.textContent = "All orders";
  select.appendChild(all);
  for (const sequence of [...sequences].sort((a, b) => a - b)) {
    const option = document.createElement("option");
    option.value = String(sequence);
    option.textContent = String(sequence);
    if (String(sequence) === current) option.selected = true;
    select.appendChild(option);
  }
}

function renderEventDetail(event) {
  if (!event) return;
  setText("event-detail-title", `Event ${eventLabel(event.event_id)}`);
  setText("event-detail-sub", `${event.kind} · order ${event.order_sequence} · log index ${event.event_id.log_index}`);
  const badgeElement = $("event-detail-badge");
  badgeElement.className = `badge ${event.kind === "fill" ? "good" : event.kind === "rejected" ? "bad" : "neutral"}`;
  badgeElement.textContent = event.kind;

  const body = $("event-detail-body");
  clear(body);
  const rows = [
    ["Order", event.order_sequence],
    ["Counterparty", event.counterparty_sequence ?? "—"],
    ["Side", event.side],
    ["Price", event.price],
    ["Volume", event.volume],
    ["Quote", event.quote_volume],
    ["Taker fee", `${event.taker_fee} ${event.taker_fee_asset ?? ""}`],
    ["Maker fee", `${event.maker_fee} ${event.maker_fee_asset ?? ""}`],
    ["Errors", (event.error || []).join(", ") || "—"],
    ["Reason", event.reason ?? "—"],
  ];
  for (const [label, value] of rows) {
    const row = document.createElement("div");
    row.className = "kv";
    const left = document.createElement("span");
    left.textContent = label;
    const right = document.createElement("span");
    right.textContent = value;
    row.append(left, right);
    body.appendChild(row);
  }
  $("event-json").textContent = JSON.stringify(event, null, 2);
}

function renderFullBook() {
  const book = state.fullBook;
  const bidsBody = $("book-bids-body");
  const asksBody = $("book-asks-body");
  const stopsBody = $("book-stops-body");
  clear(bidsBody);
  clear(asksBody);
  clear(stopsBody);
  clear($("book-levels-body"));
  if (!book) {
    setText("book-meta", "not loaded");
    emptyRow(bidsBody, 8, "Load the full book");
    emptyRow(asksBody, 8, "Load the full book");
    emptyRow(stopsBody, 8, "Load the full book");
    return;
  }
  const start = state.fullPage * state.pageSize;
  const bids = book.bids.slice(start, start + state.pageSize);
  const asks = book.asks.slice(start, start + state.pageSize);
  setText("book-meta", `${book.counts.bids + book.counts.asks} orders · ${book.depth.bids.length + book.depth.asks.length} levels · ${book.counts.stops} stops`);
  setText("book-bids-count", book.counts.bids);
  setText("book-asks-count", book.counts.asks);
  setText("book-stops-count", book.counts.stops);
  setText("book-levels-count", book.depth.bids.length + book.depth.asks.length);
  setText("book-snapshot", `snapshot ${book.market_seq}`);
  setText("book-loaded", `loaded ${new Date().toLocaleTimeString()}`);

  if (!bids.length) emptyRow(bidsBody, 8, "No bids");
  for (const order of bids) {
    const row = document.createElement("tr");
    cell(row, order.sequence, "mono");
    cell(row, order.price, "num mono");
    cell(row, order.volume, "num mono");
    cell(row, order.hidden_volume, "num mono");
    cell(row, order.display_volume, "num mono");
    cell(row, order.live);
    cell(row, tifLabel(order.tif));
    cell(row, flags(order), "flags");
    bidsBody.appendChild(row);
  }
  if (!asks.length) emptyRow(asksBody, 8, "No asks");
  for (const order of asks) {
    const row = document.createElement("tr");
    cell(row, order.sequence, "mono");
    cell(row, order.price, "num mono");
    cell(row, order.volume, "num mono");
    cell(row, order.hidden_volume, "num mono");
    cell(row, order.display_volume, "num mono");
    cell(row, order.live);
    cell(row, tifLabel(order.tif));
    cell(row, flags(order), "flags");
    asksBody.appendChild(row);
  }

  const levels = [...book.depth.bids.map((level) => ["Bid", level]), ...book.depth.asks.map((level) => ["Ask", level])];
  for (const [side, level] of levels) {
    const row = document.createElement("tr");
    const sideCell = document.createElement("td");
    sideCell.className = `side ${side.toLowerCase()}`;
    sideCell.textContent = side;
    row.appendChild(sideCell);
    cell(row, level.price, "num mono");
    cell(row, level.volume, "num mono");
    cell(row, level.orders, "num");
    $("book-levels-body").appendChild(row);
  }

  if (!book.stops.length) emptyRow(stopsBody, 8, "No parked stops");
  for (const order of book.stops) {
    const row = document.createElement("tr");
    cell(row, order.sequence, "mono");
    cell(row, order.side, sideClass(order.side));
    cell(row, order.stop_price, "num mono");
    cell(row, order.volume, "num mono");
    cell(row, order.live);
    cell(row, tifLabel(order.tif));
    cell(row, order.hidden_volume, "num mono");
    cell(row, order.display_volume, "num mono");
    stopsBody.appendChild(row);
  }

  const bidPages = Math.max(1, Math.ceil(book.bids.length / state.pageSize));
  const askPages = Math.max(1, Math.ceil(book.asks.length / state.pageSize));
  const pages = Math.max(bidPages, askPages);
  setText("book-page", `page ${state.fullPage + 1} / ${pages}`);
  setText("book-page-asks", `page ${state.fullPage + 1} / ${pages}`);
  $("book-prev").disabled = state.fullPage === 0;
  $("book-prev-asks").disabled = state.fullPage === 0;
  $("book-next").disabled = state.fullPage + 1 >= pages;
  $("book-next-asks").disabled = state.fullPage + 1 >= pages;
}

function flags(order) {
  const out = [];
  if (order.post_only) out.push("post-only");
  if (order.stop_price && order.stop_price !== "0") out.push(`stop ${order.stop_price}`);
  if (order.display_volume && order.display_volume !== "0") out.push(`iceberg ${order.display_volume}`);
  return out.length ? out.join(" · ") : "—";
}

function renderAll() {
  renderNode();
  renderSymbol();
  renderBook();
  renderOrders();
  renderStops();
  renderRecentEvents();
  renderEvents();
}

async function refreshState() {
  if (state.stateLoading || document.hidden) return;
  state.stateLoading = true;
  try {
    const response = await fetchJSON("/demo/api/state?depth=20");
    state.node = response.node;
    state.symbol = response.symbol;
    state.book = response.book;
    state.orders = response.orders.items;
    state.stops = response.stops.items;
    state.stateOrdersTotal = response.orders.total;
    if (!state.cursor && state.symbol && state.node) {
      restoreCursor();
    }
    renderAll();
  } catch (error) {
    showWarning("State unavailable", error.message);
  } finally {
    state.stateLoading = false;
    if ($("auto-refresh").checked) {
      clearTimeout(state.stateTimer);
      state.stateTimer = setTimeout(refreshState, 1000);
    }
  }
}

function cursorStorageKey() {
  if (!state.node || !state.symbol) return null;
  return `demo-cursor:${state.node.id}:${state.symbol.config.symbol_id}`;
}

function restoreCursor() {
  const key = cursorStorageKey();
  if (!key) return;
  try {
    const raw = sessionStorage.getItem(key);
    if (raw) state.cursor = JSON.parse(raw);
  } catch (_) {
    state.cursor = null;
  }
}

function saveCursor() {
  const key = cursorStorageKey();
  if (!key) return;
  if (state.cursor) sessionStorage.setItem(key, JSON.stringify(state.cursor));
  else sessionStorage.removeItem(key);
}

async function refreshEvents(reset = false) {
  if (state.eventsLoading || state.paused || document.hidden) return;
  state.eventsLoading = true;
  try {
    if (reset) {
      state.events.clear();
      state.cursor = null;
      saveCursor();
    }
    const limit = Number($("event-limit").value || 500);
    let cursor = state.cursor;
    let resetOnce = false;
    for (let page = 0; page < 20; page += 1) {
      const query = cursor
        ? `?after_log_index=${cursor.log_index}&after_ordinal=${cursor.ordinal}&limit=${limit}`
        : `?limit=${limit}`;
      const response = await fetchJSON(`/demo/api/events${query}`);
      if (cursor && response.oldest_event_id && compareId(response.oldest_event_id, cursor) > 0) {
        if (resetOnce) break;
        resetOnce = true;
        state.events.clear();
        cursor = null;
        state.cursor = null;
        showWarning("Event history trimmed", "The cursor fell behind the retained window; reloaded from the oldest event.");
        continue;
      }
      for (const event of response.events) {
        state.events.set(eventKey(event.event_id), event);
      }
      const next = response.next_after;
      if (next && cursor && compareId(next, cursor) === 0) break;
      if (next) cursor = next;
      state.cursor = cursor;
      saveCursor();
      if (!response.has_more) break;
    }
    pruneEvents();
    renderRecentEvents();
    renderEvents();
    if (state.selectedEvent) {
      const fresh = state.events.get(eventKey(state.selectedEvent.event_id));
      if (fresh) {
        state.selectedEvent = fresh;
        renderEventDetail(fresh);
      }
    }
  } catch (error) {
    showWarning("Event feed unavailable", error.message);
  } finally {
    state.eventsLoading = false;
    if ($("auto-refresh").checked && !state.paused) {
      clearTimeout(state.eventTimer);
      state.eventTimer = setTimeout(() => refreshEvents(false), 700);
    }
  }
}

function pruneEvents() {
  if (state.events.size <= 5000) return;
  const keys = [...state.events.keys()].slice(0, state.events.size - 5000);
  for (const key of keys) state.events.delete(key);
}

async function loadFullBook() {
  try {
    const book = await fetchJSON("/demo/api/book");
    state.fullBook = book;
    state.fullPage = 0;
    renderFullBook();
  } catch (error) {
    showWarning("Full book unavailable", error.message);
  }
}

function ticketSide(side) {
  state.side = side;
  $("side-buy").classList.toggle("active", side === "Buy");
  $("side-sell").classList.toggle("active", side === "Sell");
  $("side-buy").classList.toggle("buy", true);
  $("side-sell").classList.toggle("sell", true);
  $("ticket-submit").textContent = `Place ${side.toLowerCase()} order`;
}

function ticketTouched() {
  setText("ticket-subtitle", `${$("ticket-type").value} · ${$("ticket-tif").value}`);
  setText("ticket-type-badge", $("ticket-type").value);
  $("expiry-field").hidden = $("ticket-tif").value !== "GTD";
  const market = $("ticket-type").value === "Market";
  $("ticket-price").disabled = market;
  if (market) $("ticket-price").value = "";
  $("ticket-post-only").disabled = market;
  $("ticket-display").disabled = market;
  $("ticket-stop").disabled = market;
}

function clientId() {
  if (window.crypto && typeof window.crypto.randomUUID === "function") {
    return `demo-${window.crypto.randomUUID()}`;
  }
  return `demo-${Date.now()}-${Math.random().toString(16).slice(2)}`;
}

function ensureClientId() {
  if (!$("ticket-client-id").value) $("ticket-client-id").value = clientId();
}

function showResult(result, bad = false) {
  const banner = $("ticket-result");
  banner.hidden = false;
  banner.classList.toggle("bad-result", bad);
  if (bad) {
    setText("result-title", "Rejected");
    setText("result-text", result);
  } else {
    const rejected = (result.events || []).find((event) => event.kind === "rejected");
    setText("result-title", rejected ? `Rejected · sequence ${result.sequence ?? "—"}` : `Accepted · sequence ${result.sequence ?? "—"}`);
    banner.classList.toggle("bad-result", Boolean(rejected || result.error));
    const detail = result.error || rejected?.reason || `${result.events?.length ?? 0} event(s) · log_index ${result.log_index}`;
    setText("result-text", detail);
  }
  const body = $("result-events-body");
  clear(body);
  for (const event of result.events || []) body.appendChild(eventRow(event, true));
  if (!(result.events || []).length) emptyRow(body, 6, "No events returned");
}

async function submitTicket(event) {
  event.preventDefault();
  ensureClientId();
  const price = $("ticket-price").value.trim();
  const volume = $("ticket-volume").value.trim();
  if (!volume) {
    showResult("Volume is required", true);
    return;
  }
  const body = {
    side: state.side,
    type: $("ticket-type").value,
    tif: $("ticket-tif").value,
    volume,
    volume_unit: $("ticket-unit").value,
    post_only: $("ticket-post-only").checked,
    maker_fee_rate: $("ticket-maker").value.trim() || "0",
    taker_fee_rate: $("ticket-taker").value.trim() || "0",
    client_order_id: $("ticket-client-id").value,
  };
  if (price) body.price = price;
  const stop = $("ticket-stop").value.trim();
  if (stop) body.stop_price = stop;
  if ($("ticket-iceberg").checked) {
    const display = $("ticket-display").value.trim();
    if (display) body.display_volume = display;
  }
  if ($("ticket-tif").value === "GTD") {
    const expiry = Date.parse($("ticket-expiry").value);
    if (!Number.isFinite(expiry)) {
      showResult("GTD expiry is required", true);
      return;
    }
    body.expiry_ms = expiry;
  }

  $("ticket-submit").disabled = true;
  try {
    const result = await postJSON("/demo/api/orders", body);
    showResult(result);
    $("ticket-client-id").value = clientId();
    await refreshState();
    await refreshEvents(false);
  } catch (error) {
    showResult(error.message, true);
  } finally {
    $("ticket-submit").disabled = false;
  }
}

async function cancelOrder(order) {
  if (!window.confirm(`Cancel order ${order.sequence}?`)) return;
  try {
    const result = await postJSON("/demo/api/orders/cancel", {
      side: order.side,
      price: order.price,
      sequence: order.sequence,
    });
    showResult(result);
    await refreshState();
    await refreshEvents(false);
  } catch (error) {
    showResult(error.message, true);
  }
}

async function amendOrder(order) {
  const price = window.prompt("New price (leave blank to keep)", "");
  if (price === null) return;
  const volume = window.prompt("New open volume (leave blank to keep)", "");
  if (volume === null) return;
  if (!price && !volume) return;
  const body = {
    side: order.side,
    price: order.price,
    sequence: order.sequence,
  };
  if (price) body.new_price = price;
  if (volume) body.new_volume = volume;
  try {
    const result = await postJSON("/demo/api/orders/amend", body);
    showResult(result);
    await refreshState();
    await refreshEvents(false);
  } catch (error) {
    showResult(error.message, true);
  }
}

async function submitSymbol(event) {
  event.preventDefault();
  const body = {
    symbol_id: Number($("config-symbol-id").textContent || 0),
    base_asset: {
      code: $("symbol-base-code").value,
      class: $("symbol-base-class").value,
      decimals: Number($("symbol-base-decimals").value),
    },
    quote_asset: {
      code: $("symbol-quote-code").value,
      class: $("symbol-quote-class").value,
      decimals: Number($("symbol-quote-decimals").value),
    },
    price_decimals: Number($("symbol-price-decimals").value),
    price_tick: $("symbol-tick").value,
    base_lot: $("symbol-lot").value,
    fee_asset_policy: $("symbol-policy").value,
  };
  try {
    const result = await postJSON("/demo/api/symbol", body);
    showResult(result);
    state.symbolFormReady = false;
    await refreshState();
  } catch (error) {
    showResult(error.message, true);
  }
}

function bind() {
  $("refresh-button").addEventListener("click", () => {
    refreshState();
    refreshEvents(false);
    if (!$("tab-storage").checked) loadFullBook();
  });
  $("manual-refresh").addEventListener("click", () => {
    refreshState();
    refreshEvents(false);
  });
  $("auto-refresh").addEventListener("change", () => {
    clearTimeout(state.stateTimer);
    clearTimeout(state.eventTimer);
    if ($("auto-refresh").checked) {
      refreshState();
      refreshEvents(false);
    }
  });
  $("side-buy").addEventListener("click", () => ticketSide("Buy"));
  $("side-sell").addEventListener("click", () => ticketSide("Sell"));
  $("ticket-type").addEventListener("change", ticketTouched);
  $("ticket-tif").addEventListener("change", ticketTouched);
  $("ticket-form").addEventListener("submit", submitTicket);
  $("symbol-form").addEventListener("submit", submitSymbol);
  $("event-kind").addEventListener("change", renderEvents);
  $("event-side").addEventListener("change", renderEvents);
  $("event-order").addEventListener("change", renderEvents);
  $("event-limit").addEventListener("change", () => refreshEvents(true));
  $("events-pause").addEventListener("click", () => {
    state.paused = !state.paused;
    $("events-pause").textContent = state.paused ? "Resume" : "Pause";
    if (!state.paused) refreshEvents(false);
    renderEvents();
  });
  $("events-reset").addEventListener("click", () => {
    state.events.clear();
    state.cursor = null;
    saveCursor();
    renderEvents();
    renderRecentEvents();
    refreshEvents(true);
  });
  $("tab-storage").addEventListener("change", () => {
    if ($("tab-storage").checked) loadFullBook();
  });
  $("tab-events").addEventListener("change", () => {
    if ($("tab-events").checked) refreshEvents(false);
  });
  $("book-refresh").addEventListener("click", loadFullBook);
  $("book-prev").addEventListener("click", () => {
    if (state.fullPage > 0) state.fullPage -= 1;
    renderFullBook();
  });
  $("book-next").addEventListener("click", () => {
    state.fullPage += 1;
    renderFullBook();
  });
  $("book-prev-asks").addEventListener("click", () => $("book-prev").click());
  $("book-next-asks").addEventListener("click", () => $("book-next").click());
  $("book-copy").addEventListener("click", async () => {
    if (!state.fullBook) return;
    try {
      await navigator.clipboard.writeText(JSON.stringify(state.fullBook, null, 2));
      setText("book-loaded", "copied JSON");
    } catch (error) {
      showWarning("Clipboard unavailable", error.message);
    }
  });
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) {
      refreshState();
      refreshEvents(false);
    }
  });

  ticketSide(state.side);
  ticketTouched();
  ensureClientId();
}

bind();
refreshState();
refreshEvents(false);
