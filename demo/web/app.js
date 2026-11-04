function emptyView() {
  return {
    node: null,
    symbol: null,
    book: null,
    orders: [],
    stops: [],
    events: new Map(),
    cursor: null,
    stateOrdersTotal: 0,
    fullBook: null,
    fullPage: 0,
    selectedEvent: null,
    selectedOrder: null,
    freeze: null,
    symbolFormReady: false,
  };
}

const state = {
  product: "spot",
  backends: null,
  views: { spot: emptyView(), perpetual: emptyView() },
  side: "Buy",
  paused: false,
  pageSize: 50,
  ticketPreview: null,
  ticketPreviewSeq: 0,
  ticketPreviewTimer: null,
  stateLoading: new Set(),
  eventLoading: new Set(),
  stateTimer: null,
  eventTimer: null,
};

// Each product has its own engine, symbol, book, order set, event window and
// cursor. These accessors make the active view the `state.*` fields the render
// code already uses; switching products preserves the inactive view.
for (const key of [
  "node", "symbol", "book", "orders", "stops", "events", "cursor",
  "stateOrdersTotal", "fullBook", "fullPage", "selectedEvent", "selectedOrder",
  "freeze", "symbolFormReady",
]) {
  Object.defineProperty(state, key, {
    get() { return state.views[state.product][key]; },
    set(value) { state.views[state.product][key] = value; },
  });
}

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

function amountLabel(amount) {
  if (!amount || amount.amount === undefined || amount.amount === null) return "—";
  return `${amount.amount} ${amount.asset}`;
}

function basisLabel(basis) {
  const labels = {
    quote_notional: "quote notional",
    base_principal: "base principal",
    quote_notional_plus_fee: "quote notional + fee",
    quote_collateral_plus_fee: "quote collateral + base-fee reserve",
    perpetual_margin_plus_fee: "perpetual margin + quote fee",
  };
  return labels[basis] || basis || "—";
}

function roleLabel(role) {
  return role === "taker" ? "Taker" : role === "maker" ? "Maker" : role || "—";
}

function percentLabel(rate) {
  const value = Number(rate);
  if (!Number.isFinite(value)) return rate === null || rate === undefined ? "—" : String(rate);
  return `${Number((value * 100).toFixed(6))}%`;
}

function fundingLabel(funding) {
  const labels = {
    withhold_from_base_delivery: "Base fee is withheld from the base delivery.",
    withhold_from_quote_proceeds: "Quote fee is withheld from the quote proceeds.",
    deduct_from_quote_freeze: "Quote fee is deducted from the frozen quote.",
    base_from_base_delivery: "Base fee is withheld from the base delivery; the matching quote reserve is released.",
    base_against_quote_collateral: "Base fee is withheld or acquired against the quote collateral; the matching quote reserve is released.",
    deduct_from_margin: "Quote fee is deducted from the frozen perpetual margin.",
  };
  return labels[funding] || "Fee funding follows the symbol's fee-asset policy.";
}

function orderFreeze(order) {
  return order && order.economics ? order.economics.freeze : null;
}

function freezeLabel(order) {
  const freeze = orderFreeze(order);
  return freeze ? `${freeze.amount} ${freeze.asset}` : "—";
}

function productLabel(entry) {
  if (!entry || entry.product !== "perpetual") return "Spot";
  return `Perp\u00a0${entry.leverage ?? "?"}x`;
}

function orderTitle(entry) {
  const order = entry.order || entry;
  const base = state.symbol?.config.base_asset.code ?? "";
  const price = order.price && order.price !== "0" ? `@ ${order.price}` : "";
  const stop = order.stop_price && order.stop_price !== "0" ? `stop ${order.stop_price}` : "";
  const style = order.live === "Market" ? "market" : "";
  const product = entry.product === "perpetual" ? `${productLabel(entry)} ` : "";
  return `${product}${order.side} ${order.volume} ${base} ${price || stop || style}`.trim();
}

function freezeSummaryText(freezes) {
  if (!freezes || !freezes.length) return "freeze —";
  return `freeze ${freezes.map((item) => `${item.amount} ${item.asset}`).join(" · ")}`;
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

function productName(product = state.product) {
  return product === "perpetual" ? "Perpetual" : "Spot";
}

function activeBackend() {
  return state.backends?.[state.product] ?? null;
}

function pairLabel(product) {
  const config = state.views[product].symbol?.config;
  if (config) {
    return `${config.base_asset.code}/${config.quote_asset.code} · ${productName(product)}`;
  }
  const backend = state.backends?.[product];
  return backend
    ? `${productName(product)} · ${backend.http_addr}`
    : `${productName(product)} · not configured`;
}

function renderPairSelect() {
  const select = $("pair-select");
  if (!select) return;
  if (!state.backends) {
    if (select.dataset.signature !== "loading") {
      const option = document.createElement("option");
      option.textContent = "Loading pairs…";
      option.disabled = true;
      select.replaceChildren(option);
      select.dataset.signature = "loading";
    }
    return;
  }
  const options = ["spot", "perpetual"].map((product) => ({
    value: product,
    label: pairLabel(product),
    disabled: !state.backends[product],
  }));
  const signature = JSON.stringify(options);
  if (select.dataset.signature !== signature) {
    select.replaceChildren();
    for (const item of options) {
      const option = document.createElement("option");
      option.value = item.value;
      option.textContent = item.label;
      option.disabled = item.disabled;
      select.appendChild(option);
    }
    select.dataset.signature = signature;
  }
  if (state.backends?.[state.product]) select.value = state.product;
}

function renderBackendHint() {
  const hint = $("pair-hint");
  if (!hint) return;
  const spot = state.backends?.spot;
  const perpetual = state.backends?.perpetual;
  if (!spot && !perpetual) {
    hint.textContent = "Connecting to engine backends…";
    return;
  }
  if (!perpetual) {
    hint.textContent = `Spot-only desk: ${pairLabel("spot")} on ${spot?.http_addr ?? "—"}.`;
    return;
  }
  hint.textContent =
    `Separate engines and orderbooks: ${pairLabel("spot")} on ${spot?.http_addr ?? "—"}; ` +
    `${pairLabel("perpetual")} on ${perpetual.http_addr}.`;
}

function renderBackends() {
  const spot = state.backends?.spot;
  const perpetual = state.backends?.perpetual;
  setText("node-product", productName());
  setText("node-spot-engine", spot ? spot.http_addr : "—");
  setText("node-perp-engine", perpetual ? perpetual.http_addr : "not configured");
  setChip(
    "backend-chip",
    `${productName()} · ${activeBackend()?.http_addr ?? "not connected"}`,
    activeBackend() ? "neutral" : "bad",
  );
  renderPairSelect();
  renderBackendHint();
  renderTicketLabels();
  renderProductHint();
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
  setText("symbol-chip", state.symbol ? `${productName()} ${state.symbol.config.base_asset.code}/${state.symbol.config.quote_asset.code}` : "Symbol —");
  setChip("backend-chip", `${productName()} · ${node.http_addr}`, "neutral");
  setText("updated", nowLabel());
  setText("node-product", productName());
  setText("node-engine", node.http_addr);
  setText("node-leader-term", `${node.leader_id ?? "—"} / ${node.term}`);
  setText("node-raft-state", node.raft_state);
  setText("node-book-seq", state.book?.sequence ?? "—");
  $("node-ready-badge").className = `badge ${node.ready ? "good" : "bad"}`;
  $("node-ready-badge").textContent = node.ready ? "Ready" : "Not ready";

  if (!node.healthy) {
    showWarning(`${productName()} backend unhealthy`, node.ready ? "Storage or Raft is degraded." : "The node is not ready.");
  } else if (!node.is_leader) {
    showWarning(
      "Follower view",
      node.leader_addr
        ? `${productName()} book reads may lag. Writes and events follow leader ${node.leader_id} on ${node.leader_addr}.`
        : `${productName()} book reads may lag. Writes and events follow leader ${node.leader_id ?? "unknown"}.`,
    );
  } else {
    hideWarning();
  }
}

function renderSymbol() {
  const symbol = state.symbol;
  if (!symbol) return;
  const config = symbol.config;
  renderPairSelect();
  setText("instrument-subtitle", `${productName()} · ${config.base_asset.code} / ${config.quote_asset.code} · ${config.base_asset.class} / ${config.quote_asset.class}`);
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
  renderTicketLabels();
  renderProductHint();
  renderBackendHint();

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

function orderTypeLabel(entry) {
  const order = entry.order || entry;
  const bits = [];
  if (entry.product === "perpetual") bits.push(productLabel(entry));
  bits.push(order.live);
  const tif = tifLabel(order.tif);
  if (tif !== "GTC") bits.push(tif);
  if (order.post_only) bits.push("post-only");
  if (order.display_volume && order.display_volume !== "0") bits.push(`iceberg ${order.display_volume}`);
  if (order.stop_price && order.stop_price !== "0") bits.push(`stop ${order.stop_price}`);
  return bits.join(" · ");
}

function tableTypeLabel(entry) {
  // Open orders always rest, so the product tag is the useful part for a
  // perpetual row; the full label would wrap in the narrow Type column.
  if (entry.product === "perpetual") return productLabel(entry);
  return orderTypeLabel(entry);
}

function renderOrders() {
  const body = $("orders-body");
  clear(body);
  const items = state.orders || [];
  setText("orders-count", `${items.length}/${state.stateOrdersTotal ?? items.length}`);
  if (!items.length) {
    emptyRow(body, 7, "No resting orders");
    return;
  }
  for (const order of items) {
    const row = document.createElement("tr");
    row.classList.toggle("selected", state.selectedOrder?.order?.sequence === order.order.sequence);
    row.addEventListener("click", (event) => {
      if (event.target.closest("button")) return;
      selectOrder(order);
    });
    cell(row, order.order.sequence, "mono");
    cell(row, order.order.side, sideClass(order.order.side));
    cell(row, order.order.price, "mono");
    cell(row, order.order.volume, "mono");
    cell(row, freezeLabel(order), "num mono freeze-cell");
    cell(row, tableTypeLabel(order));
    const actions = document.createElement("td");
    actions.className = "actions";
    const cancel = document.createElement("button");
    cancel.className = "btn ghost";
    cancel.type = "button";
    cancel.textContent = "Cancel";
    cancel.addEventListener("click", () => cancelOrder(order.order));
    actions.appendChild(cancel);
    if (order.order.live === "Limit" && (!order.order.stop_price || order.order.stop_price === "0")) {
      const amend = document.createElement("button");
      amend.className = "btn";
      amend.type = "button";
      amend.textContent = "Amend";
      amend.addEventListener("click", () => amendOrder(order.order));
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
    emptyRow(body, 7, "No parked stops");
    return;
  }
  for (const order of items) {
    const row = document.createElement("tr");
    row.classList.toggle("selected", state.selectedOrder?.order?.sequence === order.order.sequence);
    row.addEventListener("click", (event) => {
      if (event.target.closest("button")) return;
      selectOrder(order);
    });
    cell(row, order.order.sequence, "mono");
    cell(row, order.order.side, sideClass(order.order.side));
    cell(row, order.order.stop_price, "mono");
    cell(row, order.order.volume, "mono");
    cell(row, freezeLabel(order), "num mono freeze-cell");
    cell(row, orderTypeLabel(order));
    const actions = document.createElement("td");
    actions.className = "actions";
    const cancel = document.createElement("button");
    cancel.className = "btn ghost";
    cancel.type = "button";
    cancel.textContent = "Cancel";
    cancel.addEventListener("click", () => cancelOrder(order.order));
    actions.appendChild(cancel);
    row.appendChild(actions);
    body.appendChild(row);
  }
}

function renderFreezeSummary() {
  const container = $("freeze-chips");
  clear(container);
  const freeze = state.freeze;
  const total = freeze?.total || [];
  if (!total.length) {
    const empty = document.createElement("span");
    empty.className = "freeze-empty";
    empty.textContent = "No priced open orders";
    container.appendChild(empty);
  } else {
    for (const item of total) {
      const chip = document.createElement("span");
      chip.className = "freeze-chip";
      chip.title = `${item.orders} order(s)`;
      const amount = document.createElement("strong");
      amount.textContent = item.amount;
      const asset = document.createElement("span");
      asset.textContent = item.asset;
      chip.append(amount, asset);
      container.appendChild(chip);
    }
  }
  const unpriced = freeze?.unestimated ? ` · ${freeze.unestimated} unpriced` : "";
  setText(
    "freeze-note",
    freeze
      ? `${freeze.estimated} priced${unpriced} · demo estimate · engine holds no balances`
      : "demo estimate · engine holds no balances",
  );
}

function selectOrder(order) {
  state.selectedOrder = order;
  renderOrders();
  renderStops();
  renderEconomics();
  if ($("tab-storage").checked) $("tab-overview").checked = true;
  $("economics-panel").scrollIntoView({ behavior: "smooth", block: "nearest" });
}

function settlementRow(leg, roleText) {
  const row = document.createElement("tr");
  const role = document.createElement("td");
  role.appendChild(badge(roleText || roleLabel(leg.role), leg.role === "taker" ? "leader" : "neutral"));
  row.appendChild(role);
  cell(row, amountLabel(leg.delivers), "mono");
  cell(row, amountLabel(leg.receives), "mono");
  const fee = cell(row, amountLabel(leg.fee), "num mono");
  if (Number(leg.fee.amount) < 0) fee.classList.add("rebate");
  cell(row, percentLabel(leg.fee_rate), "num mono");
  return row;
}

function renderEconomics() {
  const selected = state.selectedOrder;
  const detail = $("economics-detail");
  const empty = $("economics-empty");
  const clearButton = $("economics-clear");
  const badgeElement = $("economics-badge");
  if (!selected) {
    detail.hidden = true;
    empty.hidden = false;
    clearButton.hidden = true;
    setText("economics-title", "Order economics");
    setText("economics-sub", "Select an open order, a parked stop or a full-book row.");
    badgeElement.className = "badge neutral";
    badgeElement.textContent = "estimate";
    return;
  }

  const order = selected.order;
  const economics = selected.economics;
  const base = state.symbol?.config.base_asset.code ?? "";
  detail.hidden = false;
  empty.hidden = true;
  clearButton.hidden = false;
  setText("economics-title", orderTitle(selected));
  setText(
    "economics-sub",
    `Sequence ${order.sequence} · ${orderTypeLabel(selected)} · ${order.side} ${order.volume} ${base}`,
  );
  badgeElement.className = "badge leader";
  badgeElement.textContent = "demo estimate";

  const body = $("settlement-body");
  clear(body);
  const perpTerms = $("perp-terms");
  const settlementWrap = $("settlement-wrap");
  perpTerms.hidden = true;
  settlementWrap.hidden = false;
  if (!economics) {
    setText("economics-freeze-value", "—");
    setText("economics-freeze-basis", "—");
    setText("economics-freeze-rate", "—");
    setText("economics-reference", "—");
    emptyRow(body, 5, "No estimate: this order has no reference price.");
    setText(
      "economics-note",
      "The engine does not hold balances or enforce a freeze; this is a demo estimate.",
    );
    return;
  }

  const freeze = economics.freeze;
  setText("economics-freeze-value", `${freeze.amount} ${freeze.asset}`);
  setText("economics-freeze-basis", `${roleLabel(freeze.role)} · ${basisLabel(freeze.basis)}`);
  setText("economics-freeze-rate", percentLabel(freeze.fee_rate));
  setText("economics-reference", freeze.reference_price);
  if (economics.perpetual) {
    const perpetual = economics.perpetual;
    settlementWrap.hidden = true;
    perpTerms.hidden = false;
    setText("perp-product", "Perpetual · linear");
    setText("perp-leverage", `${perpetual.leverage}x`);
    setText("perp-position", perpetual.position === "short" ? "Short" : "Long");
    setText("perp-notional", `${perpetual.notional} ${freeze.asset}`);
    setText("perp-margin", `${perpetual.initial_margin} ${freeze.asset}`);
    setText("perp-fee", amountLabel(perpetual.fee));
    setText("perp-buffer", `${perpetual.buffer} ${freeze.asset}`);
    setText("perp-margin-asset", freeze.asset);
    setText("perp-rate", percentLabel(freeze.fee_rate));
    setText(
      "economics-note",
      `Linear USDT-margined perpetual demo overlay. ${fundingLabel(economics.fee_funding)} The engine matches and reports the fills; it does not hold margin, fund, compute PnL or liquidate. Demo estimate only.`,
    );
    return;
  }
  if (economics.maker) body.appendChild(settlementRow(economics.maker, "Maker (resting)"));
  if (economics.taker) {
    body.appendChild(
      settlementRow(economics.taker, economics.maker ? "Taker (immediate)" : "Taker (trigger/market)"),
    );
  }
  setText(
    "economics-note",
    `Legs are from this order's perspective; the counterparty delivers what this order receives. ${fundingLabel(economics.fee_funding)} A negative fee is a rebate. Demo estimate only: the engine reports fill fees, not balances.`,
  );
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
    setText("book-freeze", "freeze —");
    emptyRow(bidsBody, 9, "Load the full book");
    emptyRow(asksBody, 9, "Load the full book");
    emptyRow(stopsBody, 9, "Load the full book");
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
  setText("book-freeze", freezeSummaryText(book.freeze?.total));
  setText("book-loaded", `loaded ${new Date().toLocaleTimeString()}`);

  if (!bids.length) emptyRow(bidsBody, 9, "No bids");
  for (const entry of bids) {
    const order = entry.order;
    const row = document.createElement("tr");
    row.classList.toggle("selected", state.selectedOrder?.order?.sequence === order.sequence);
    row.addEventListener("click", () => selectOrder(entry));
    cell(row, order.sequence, "mono");
    cell(row, order.price, "num mono");
    cell(row, order.volume, "num mono");
    cell(row, order.hidden_volume, "num mono");
    cell(row, order.display_volume, "num mono");
    cell(row, freezeLabel(entry), "num mono freeze-cell");
    cell(row, orderTypeLabel(entry));
    cell(row, tifLabel(order.tif));
    cell(row, flags(order), "flags");
    bidsBody.appendChild(row);
  }
  if (!asks.length) emptyRow(asksBody, 9, "No asks");
  for (const entry of asks) {
    const order = entry.order;
    const row = document.createElement("tr");
    row.classList.toggle("selected", state.selectedOrder?.order?.sequence === order.sequence);
    row.addEventListener("click", () => selectOrder(entry));
    cell(row, order.sequence, "mono");
    cell(row, order.price, "num mono");
    cell(row, order.volume, "num mono");
    cell(row, order.hidden_volume, "num mono");
    cell(row, order.display_volume, "num mono");
    cell(row, freezeLabel(entry), "num mono freeze-cell");
    cell(row, orderTypeLabel(entry));
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

  if (!book.stops.length) emptyRow(stopsBody, 9, "No parked stops");
  for (const entry of book.stops) {
    const order = entry.order;
    const row = document.createElement("tr");
    row.classList.toggle("selected", state.selectedOrder?.order?.sequence === order.sequence);
    row.addEventListener("click", () => selectOrder(entry));
    cell(row, order.sequence, "mono");
    cell(row, order.side, sideClass(order.side));
    cell(row, order.stop_price, "num mono");
    cell(row, order.volume, "num mono");
    cell(row, freezeLabel(entry), "num mono freeze-cell");
    cell(row, orderTypeLabel(entry));
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
  renderFreezeSummary();
  renderEconomics();
  renderRecentEvents();
  renderEvents();
}

async function refreshState() {
  const product = state.product;
  if (state.stateLoading.has(product) || document.hidden) return;
  state.stateLoading.add(product);
  try {
    const response = await fetchJSON(`/demo/api/state?depth=20&product=${product}`);
    if (product !== state.product) return;
    state.backends = response.backends || state.backends;
    state.node = response.node;
    state.symbol = response.symbol;
    state.book = response.book;
    state.orders = response.orders.items;
    state.stops = response.stops.items;
    state.freeze = response.freeze;
    state.stateOrdersTotal = response.orders.total;
    if (state.selectedOrder) {
      const wanted = state.selectedOrder.order.sequence;
      const fresh = [
        ...state.orders,
        ...state.stops,
        ...(state.fullBook?.bids || []),
        ...(state.fullBook?.asks || []),
        ...(state.fullBook?.stops || []),
      ].find((entry) => entry.order.sequence === wanted);
      state.selectedOrder = fresh || null;
    }
    if (!state.cursor && state.symbol && state.node) {
      restoreCursor();
    }
    renderBackends();
    renderAll();
  } catch (error) {
    if (product === state.product) {
      showWarning(`${productName(product)} backend unavailable`, error.message);
    }
  } finally {
    state.stateLoading.delete(product);
    if (product === state.product && $("auto-refresh").checked) {
      clearTimeout(state.stateTimer);
      state.stateTimer = setTimeout(refreshState, 1000);
    }
  }
}

async function refreshBackends() {
  try {
    const response = await fetchJSON("/demo/api/backends");
    state.backends = response;
  } catch (_) {
    // The state poll also carries the backend metadata; keep trying there.
  }
  renderBackends();
  refreshPairLabels();
}

async function refreshPairLabels() {
  if (!state.backends) return;
  const pending = ["spot", "perpetual"].filter(
    (product) => state.backends[product] && !state.views[product].symbol,
  );
  if (!pending.length) return;
  await Promise.all(pending.map(async (product) => {
    try {
      const response = await fetchJSON(`/demo/api/state?depth=0&product=${product}`);
      if (response?.symbol) state.views[product].symbol = response.symbol;
    } catch (_) {
      // Leave the address-based fallback label in place.
    }
  }));
  renderBackends();
}

function cursorStorageKey() {
  if (!state.node || !state.symbol) return null;
  return `demo-cursor:${state.product}:${state.node.id}:${state.symbol.config.symbol_id}`;
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
  const product = state.product;
  if (state.eventLoading.has(product) || state.paused || document.hidden) return;
  state.eventLoading.add(product);
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
        ? `after_log_index=${cursor.log_index}&after_ordinal=${cursor.ordinal}&limit=${limit}&product=${product}`
        : `limit=${limit}&product=${product}`;
      const response = await fetchJSON(`/demo/api/events?${query}`);
      if (product !== state.product) return;
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
    if (product === state.product) {
      showWarning(`${productName(product)} event feed unavailable`, error.message);
    }
  } finally {
    state.eventLoading.delete(product);
    if (product === state.product && $("auto-refresh").checked && !state.paused) {
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
  const product = state.product;
  try {
    const book = await fetchJSON(`/demo/api/book?product=${product}`);
    if (product !== state.product) return;
    state.fullBook = book;
    state.fullPage = 0;
    renderFullBook();
  } catch (error) {
    if (product === state.product) {
      showWarning(`${productName(product)} full book unavailable`, error.message);
    }
  }
}

function renderPending() {
  const backend = activeBackend();
  renderPairSelect();
  renderBackendHint();
  setText("instrument-subtitle", backend ? `Connecting to ${backend.http_addr}…` : "Backend not configured");
  setText("symbol-chip", `${productName()} —`);
  for (const id of [
    "best-bid", "best-bid-foot", "best-ask", "best-ask-foot", "spread",
    "last-trade", "last-trade-foot", "book-sequence", "tick-lot",
    "footer-book", "node-engine",
  ]) {
    setText(id, "—");
  }
  setText("node-engine", backend ? backend.http_addr : "—");
  emptyRow($("depth-body"), 4, "Connecting…");
  emptyRow($("orders-body"), 7, "Connecting…");
  emptyRow($("stops-body"), 7, "Connecting…");
  setText("orders-count", "0");
  setText("stops-count", "0");
  clear($("freeze-chips"));
  setText("freeze-note", "demo estimate · engine holds no balances");
  renderEconomics();
  renderFullBook();
  renderRecentEvents();
  renderEvents();
  renderTicketLabels();
  renderProductHint();
}

function ticketProduct(product) {
  if (product === "perpetual" && !state.backends?.perpetual) return;
  const changed = state.product !== product;
  state.product = product;
  const perpetual = product === "perpetual";
  renderPairSelect();
  $("pair-select").value = product;
  $("leverage-row").hidden = !perpetual;
  if (changed) {
    $("ticket-result").hidden = true;
    clear($("result-events-body"));
  }
  renderBackends();
  if (state.node) renderAll();
  else renderPending();
  if (state.selectedEvent) renderEventDetail(state.selectedEvent);
  else {
    setText("event-detail-title", "Select an event");
    setText("event-detail-sub", "Click a row in the tape.");
    clear($("event-detail-body"));
    $("event-json").textContent = "{}";
    $("event-detail-badge").className = "badge neutral";
    $("event-detail-badge").textContent = "—";
  }
  scheduleTicketPreview(0);
  if (changed) {
    refreshState();
    refreshEvents(false);
    if ($("tab-storage").checked) loadFullBook();
  }
}

function ticketLeverage() {
  return Number($("ticket-leverage").value || 1);
}

function renderTicketLabels() {
  const perpetual = state.product === "perpetual";
  const type = $("ticket-type").value;
  const tif = $("ticket-tif").value;
  setText("ticket-subtitle", `${perpetual ? "Perpetual" : "Spot"} · ${type} · ${tif}`);
  setText("ticket-type-badge", perpetual ? `Perp ${type}` : type);
  const side = state.side === "Sell" ? "Sell" : "Buy";
  if (perpetual) {
    $("ticket-submit").textContent = `${side === "Buy" ? "Buy / long" : "Sell / short"} · ${ticketLeverage()}x`;
  } else {
    $("ticket-submit").textContent = `Place ${side.toLowerCase()} order`;
  }
}

function renderProductHint() {
  const hint = $("leverage-hint");
  setText("leverage-margin-asset", state.symbol?.config.quote_asset.code ?? "—");
  if (state.product !== "perpetual") {
    hint.hidden = true;
    hint.textContent = "";
    hint.classList.remove("warn");
    return;
  }
  const backend = state.backends?.perpetual;
  const policy = state.symbol?.config.fee_asset_policy;
  hint.hidden = false;
  if (!backend) {
    hint.classList.add("warn");
    hint.textContent = "Perpetual backend not configured; start the demo with --perp-engine.";
    return;
  }
  if (policy && policy !== "quote") {
    hint.classList.add("warn");
    hint.textContent = `Perpetual requires fee_asset_policy=quote (this symbol uses ${policy}). Configure the symbol before trading or switch to Spot.`;
  } else {
    hint.classList.remove("warn");
    hint.textContent = `Linear quote-margined perpetual on its own orderbook at ${backend.http_addr}. Buy opens a long, sell opens a short; the engine matches the fills but does not hold margin or fund the position.`;
  }
}

function ticketSide(side) {
  state.side = side;
  $("side-buy").classList.toggle("active", side === "Buy");
  $("side-sell").classList.toggle("active", side === "Sell");
  $("side-buy").classList.toggle("buy", true);
  $("side-sell").classList.toggle("sell", true);
  renderTicketLabels();
  scheduleTicketPreview();
}

function ticketTouched() {
  renderTicketLabels();
  $("expiry-field").hidden = $("ticket-tif").value !== "GTD";
  const market = $("ticket-type").value === "Market";
  $("ticket-price").disabled = market;
  if (market) $("ticket-price").value = "";
  $("ticket-post-only").disabled = market;
  $("ticket-display").disabled = market;
  $("ticket-stop").disabled = market;
  scheduleTicketPreview();
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
    const product = result.product ? `${productName(result.product)} · ` : "";
    setText("result-title", `${product}${rejected ? `Rejected · sequence ${result.sequence ?? "—"}` : `Accepted · sequence ${result.sequence ?? "—"}`}`);
    banner.classList.toggle("bad-result", Boolean(rejected || result.error));
    const detail = result.error || rejected?.reason || `${result.events?.length ?? 0} event(s) · log_index ${result.log_index}`;
    setText("result-text", detail);
  }
  const body = $("result-events-body");
  clear(body);
  for (const event of result.events || []) body.appendChild(eventRow(event, true));
  if (!(result.events || []).length) emptyRow(body, 6, "No events returned");
}

function ticketBody(includeClientId = false) {
  const price = $("ticket-price").value.trim();
  const volume = $("ticket-volume").value.trim();
  if (!volume) return { error: "Volume is required" };
  const body = {
    side: state.side,
    product: state.product,
    type: $("ticket-type").value,
    tif: $("ticket-tif").value,
    volume,
    volume_unit: $("ticket-unit").value,
    post_only: $("ticket-post-only").checked,
    maker_fee_rate: $("ticket-maker").value.trim() || "0",
    taker_fee_rate: $("ticket-taker").value.trim() || "0",
  };
  if (state.product === "perpetual") {
    body.leverage = ticketLeverage();
  }
  if (includeClientId) {
    ensureClientId();
    body.client_order_id = $("ticket-client-id").value;
  }
  if (price) body.price = price;
  const stop = $("ticket-stop").value.trim();
  if (stop) body.stop_price = stop;
  if ($("ticket-iceberg").checked) {
    const display = $("ticket-display").value.trim();
    if (display) body.display_volume = display;
  }
  if ($("ticket-tif").value === "GTD") {
    const expiry = Date.parse($("ticket-expiry").value);
    if (!Number.isFinite(expiry)) return { error: "GTD expiry is required" };
    body.expiry_ms = expiry;
  }
  return { body };
}

function resetTicketPreview(detail) {
  state.ticketPreview = null;
  const role = $("ticket-freeze-role");
  role.className = "badge neutral";
  role.textContent = "—";
  setText("ticket-freeze-value", "—");
  setText("ticket-freeze-detail", detail);
}

function renderTicketPreview(preview) {
  const role = $("ticket-freeze-role");
  const economics = preview.economics;
  if (!economics) {
    resetTicketPreview(preview.reason || "No estimate available.");
    return;
  }
  const freeze = economics.freeze;
  role.className = "badge leader";
  setText("ticket-freeze-value", `${freeze.amount} ${freeze.asset}`);
  if (economics.perpetual) {
    const perpetual = economics.perpetual;
    role.textContent = `${roleLabel(freeze.role)} rate`;
    setText(
      "ticket-freeze-detail",
      `${perpetual.leverage}x ${perpetual.position} · margin ${perpetual.initial_margin} ${freeze.asset} + fee ${perpetual.fee.amount} ${perpetual.fee.asset} · notional ${perpetual.notional} ${freeze.asset} · ${percentLabel(freeze.fee_rate)} · reference ${freeze.reference_price}`,
    );
    return;
  }
  role.className = economics.maker ? "badge leader" : "badge neutral";
  role.textContent = economics.maker ? "worst-case rate" : "taker rate";
  setText(
    "ticket-freeze-detail",
    `${basisLabel(freeze.basis)} · fee ${percentLabel(freeze.fee_rate)} · reference ${freeze.reference_price} · ${economics.maker ? "may rest as maker" : "taker-only"}`,
  );
}

async function refreshTicketPreview() {
  if (!state.symbol) return;
  if (!$("ticket-volume").value.trim()) {
    resetTicketPreview("Fill the ticket to preview the order-system freeze.");
    return;
  }
  const product = state.product;
  const { body, error } = ticketBody(false);
  if (error) {
    resetTicketPreview(error);
    return;
  }
  const sequence = ++state.ticketPreviewSeq;
  try {
    const preview = await postJSON("/demo/api/freeze", body);
    if (sequence !== state.ticketPreviewSeq || product !== state.product) return;
    state.ticketPreview = preview;
    renderTicketPreview(preview);
  } catch (error) {
    if (sequence !== state.ticketPreviewSeq || product !== state.product) return;
    resetTicketPreview(error.message);
  }
}

function scheduleTicketPreview(delay = 250) {
  clearTimeout(state.ticketPreviewTimer);
  state.ticketPreviewTimer = setTimeout(refreshTicketPreview, delay);
}

async function submitTicket(event) {
  event.preventDefault();
  const product = state.product;
  const { body, error } = ticketBody(true);
  if (error) {
    showResult(error, true);
    return;
  }

  $("ticket-submit").disabled = true;
  try {
    const result = await postJSON("/demo/api/orders", body);
    showResult(result);
    $("ticket-client-id").value = clientId();
    if (product === state.product) {
      await refreshState();
      await refreshEvents(false);
      scheduleTicketPreview(0);
    }
  } catch (error) {
    showResult(error.message, true);
  } finally {
    $("ticket-submit").disabled = false;
  }
}

async function cancelOrder(order) {
  if (!window.confirm(`Cancel order ${order.sequence}?`)) return;
  const product = state.product;
  try {
    const result = await postJSON("/demo/api/orders/cancel", {
      product,
      side: order.side,
      price: order.price,
      sequence: order.sequence,
    });
    showResult(result);
    if (product === state.product) {
      await refreshState();
      await refreshEvents(false);
    }
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
  const product = state.product;
  const body = {
    product,
    side: order.side,
    price: order.price,
    sequence: order.sequence,
  };
  if (price) body.new_price = price;
  if (volume) body.new_volume = volume;
  try {
    const result = await postJSON("/demo/api/orders/amend", body);
    showResult(result);
    if (product === state.product) {
      await refreshState();
      await refreshEvents(false);
    }
  } catch (error) {
    showResult(error.message, true);
  }
}

async function submitSymbol(event) {
  event.preventDefault();
  const product = state.product;
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
    const result = await postJSON(`/demo/api/symbol?product=${product}`, body);
    showResult(result);
    if (product === state.product) {
      state.symbolFormReady = false;
      await refreshState();
    }
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
  $("pair-select").addEventListener("change", (event) => ticketProduct(event.target.value));
  $("side-buy").addEventListener("click", () => ticketSide("Buy"));
  $("side-sell").addEventListener("click", () => ticketSide("Sell"));
  $("ticket-type").addEventListener("change", ticketTouched);
  $("ticket-tif").addEventListener("change", ticketTouched);
  $("ticket-leverage").addEventListener("change", () => {
    renderTicketLabels();
    scheduleTicketPreview(0);
  });
  $("ticket-form").addEventListener("submit", submitTicket);
  for (const id of [
    "ticket-price", "ticket-volume", "ticket-unit", "ticket-maker", "ticket-taker",
    "ticket-stop", "ticket-display", "ticket-post-only", "ticket-iceberg", "ticket-expiry",
  ]) {
    $(id).addEventListener("input", () => scheduleTicketPreview());
    $(id).addEventListener("change", () => scheduleTicketPreview());
  }
  $("economics-clear").addEventListener("click", () => {
    state.selectedOrder = null;
    renderOrders();
    renderStops();
    renderEconomics();
  });
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

  ticketProduct(state.product);
  ticketSide(state.side);
  ticketTouched();
  ensureClientId();
}

bind();
refreshBackends();
refreshState();
refreshEvents(false);
