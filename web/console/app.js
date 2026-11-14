(() => {
  "use strict";

  const $ = (id) => document.getElementById(id);
  const state = {
    cluster: null,
    node: null,
    actions: [],
    events: [],
    eventsAfter: null,
    eventsPaused: false,
    loading: false,
    timer: null,
  };

  const fmt = (value) => (value === null || value === undefined ? "—" : String(value));
  const fmtTime = (ms) => {
    if (!ms) return "—";
    const date = new Date(ms);
    return Number.isNaN(date.getTime()) ? "—" : date.toLocaleString();
  };
  const fmtAge = (ms) => {
    if (ms === null || ms === undefined) return "—";
    if (ms < 1000) return `${ms} ms`;
    return `${(ms / 1000).toFixed(1)} s`;
  };

  function text(id, value) {
    const element = $(id);
    if (element) element.textContent = fmt(value);
  }

  function setChip(id, label, kind) {
    const element = $(id);
    if (!element) return;
    element.className = `chip ${kind || ""}`.trim();
    element.textContent = "";
    const dot = document.createElement("span");
    dot.className = `dot ${kind || ""}`.trim();
    element.appendChild(dot);
    element.appendChild(document.createTextNode(` ${label}`));
  }

  function setBadge(id, label, kind) {
    const element = $(id);
    if (!element) return;
    element.className = `badge ${kind || ""}`.trim();
    element.textContent = label;
  }

  function clear(element) {
    while (element && element.firstChild) element.removeChild(element.firstChild);
  }

  function cell(row, textValue, className) {
    const td = document.createElement("td");
    if (className) td.className = className;
    td.textContent = fmt(textValue);
    row.appendChild(td);
    return td;
  }

  function nodeName(row, node) {
    const td = document.createElement("td");
    const wrapper = document.createElement("div");
    wrapper.className = "node-id";
    const dot = document.createElement("span");
    const snapshot = node.snapshot;
    dot.className = `dot ${!node.reachable ? "bad" : snapshot && snapshot.healthy ? "good" : "warn"}`;
    wrapper.appendChild(dot);
    const strong = document.createElement("strong");
    strong.textContent = `node-${node.node_id}`;
    wrapper.appendChild(strong);
    if (node.local) {
      const local = document.createElement("span");
      local.className = "muted";
      local.textContent = "this node";
      wrapper.appendChild(local);
    }
    td.appendChild(wrapper);
    const addr = document.createElement("div");
    addr.className = "mono muted";
    addr.textContent = node.addr;
    td.appendChild(addr);
    row.appendChild(td);
  }

  async function fetchJSON(url, options = {}) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 3000);
    try {
      const response = await fetch(url, {
        ...options,
        signal: controller.signal,
        headers: {
          Accept: "application/json",
          ...(options.headers || {}),
        },
      });
      if (!response.ok) {
        let detail = `HTTP ${response.status}`;
        try {
          const body = await response.json();
          if (body && body.error) detail = body.error;
        } catch (_) {
          // Keep the status text.
        }
        throw new Error(detail);
      }
      return await response.json();
    } finally {
      clearTimeout(timeout);
    }
  }

  function leaderSnapshot(cluster) {
    if (!cluster || cluster.leader_id === null || cluster.leader_id === undefined) return null;
    const leader = cluster.nodes.find((node) => node.node_id === cluster.leader_id);
    return leader && leader.snapshot ? leader.snapshot : null;
  }

  function lagFor(cluster, nodeId) {
    const leader = leaderSnapshot(cluster);
    if (!leader || !leader.raft || !Array.isArray(leader.raft.replication)) return null;
    const entry = leader.raft.replication.find((replica) => replica.node_id === nodeId);
    return entry ? entry.lag : null;
  }

  function renderCluster(cluster) {
    state.cluster = cluster;
    const local = cluster.nodes.find((node) => node.local);
    state.node = local && local.snapshot ? local.snapshot : null;
    const node = state.node;
    const warnings = cluster.warnings || [];
    const symbol = node && node.symbol;
    const symbolDisplay = symbol && symbol.display ? symbol.display : "— / —";
    text("node-title", node ? `node-${node.node_id} / ${node.http_addr}` : `node-${cluster.local_node_id}`);
    text("symbol-title", symbolDisplay);
    text("symbol-chip-value", symbolDisplay);
    text("symbol-id", symbol ? `symbol ${symbol.symbol_id}` : "symbol —");
    text(
      "symbol-meta",
      symbol
        ? `id ${symbol.symbol_id} · price ${symbol.price_decimals} dp · tick ${symbol.price_tick} · lot ${symbol.base_lot}`
        : "waiting for the node snapshot",
    );
    document.title = `${symbolDisplay} · MatchEngine Raft Console`;
    setChip(
      "cluster-chip",
      warnings.length === 0 ? "Cluster healthy" : "Cluster degraded",
      warnings.length === 0 ? "good" : "warn",
    );
    const leaderLabel = cluster.leader_id === null || cluster.leader_id === undefined
      ? "No leader"
      : `Leader node-${cluster.leader_id}`;
    setChip("leader-chip", leaderLabel, cluster.leader_id === null ? "warn" : "good");
    setChip("term-chip", `Term ${cluster.term}`, "");
    text("updated", `Updated ${new Date(cluster.generated_at_ms).toLocaleTimeString()}`);

    const warningBanner = $("warning-banner");
    if (warningBanner) {
      warningBanner.hidden = warnings.length === 0;
      text("warning-text", warnings.join(" · "));
    }

    const reachable = cluster.nodes.filter((node) => node.reachable).length;
    const unreachable = cluster.nodes.length - reachable;
    const notReady = cluster.nodes.filter(
      (node) => !node.reachable || (node.snapshot && !node.snapshot.ready),
    );
    let maxLag = null;
    let maxLagNode = null;
    const leader = leaderSnapshot(cluster);
    if (leader && Array.isArray(leader.raft.replication)) {
      for (const replica of leader.raft.replication) {
        if (replica.node_id === cluster.leader_id) continue;
        if (replica.lag !== null && (maxLag === null || replica.lag > maxLag)) {
          maxLag = replica.lag;
          maxLagNode = replica.node_id;
        }
      }
    }

    text("stat-leader", cluster.leader_id === null ? "none" : `node-${cluster.leader_id}`);
    text(
      "stat-leader-foot",
      cluster.leader_id === null ? "no leader" : `stable · term ${cluster.term}`,
    );
    text("stat-reachable", `${reachable} / ${cluster.nodes.length}`);
    text("stat-reachable-foot", unreachable === 0 ? "all members responding" : `${unreachable} unreachable`);
    text("stat-lag", maxLag === null ? "—" : maxLag);
    text(
      "stat-lag-foot",
      maxLagNode === null
        ? leader
          ? "no followers"
          : "leader view unavailable"
        : `node-${maxLagNode}`,
    );
    text("stat-not-ready", notReady.length);
    text(
      "stat-not-ready-foot",
      notReady.length === 0 ? "all healthy" : notReady.map((n) => `node-${n.node_id}`).join(", "),
    );
    const committed = node && node.raft ? node.raft.cluster_committed_index : null;
    text("stat-committed", committed);
    text("stat-committed-foot", node && node.raft ? `applied ${fmt(node.raft.last_applied_index)}` : "waiting");
    text("member-count", `${cluster.nodes.length} ${cluster.nodes.length === 1 ? "member" : "members"}`);

    const body = $("nodes-body");
    clear(body);
    for (const member of cluster.nodes) {
      const row = document.createElement("tr");
      nodeName(row, member);
      cell(row, member.role, "badge");
      const snapshot = member.snapshot;
      const stateCell = document.createElement("td");
      const stateBadge = document.createElement("span");
      const raftState = snapshot ? snapshot.leadership.state : "Unreachable";
      stateBadge.className = `badge ${
        raftState === "Leader" ? "leader" : raftState === "Unreachable" ? "bad" : "neutral"
      }`;
      stateBadge.textContent = raftState;
      stateCell.appendChild(stateBadge);
      row.appendChild(stateCell);

      const healthCell = document.createElement("td");
      if (snapshot) {
        const ready = document.createElement("span");
        ready.className = `badge ${snapshot.ready ? "good" : "warn"}`;
        ready.textContent = snapshot.ready ? "ready" : "not ready";
        healthCell.appendChild(ready);
        const grpc = document.createElement("div");
        grpc.className = "mono muted";
        grpc.textContent = snapshot.grpc_listening ? "gRPC up" : "gRPC down";
        healthCell.appendChild(grpc);
      } else {
        const bad = document.createElement("span");
        bad.className = "badge bad";
        bad.textContent = "no health response";
        healthCell.appendChild(bad);
      }
      row.appendChild(healthCell);

      cell(row, snapshot ? snapshot.leadership.leader_id : null, "mono");
      cell(row, snapshot ? snapshot.raft.last_log_index : null, "mono");
      cell(row, snapshot ? snapshot.raft.last_applied_index : null, "mono");
      const lag = lagFor(cluster, member.node_id);
      cell(row, member.node_id === cluster.leader_id ? "—" : lag, `mono ${lag ? "warn" : ""}`.trim());
      body.appendChild(row);
    }

    renderRaft(node);
    renderStorage(node);
    renderActions(node, cluster);
    if (node) {
      setBadge(
        "events-badge",
        node.leadership.is_leader ? "live" : "leader only",
        node.leadership.is_leader ? "good" : "neutral",
      );
    }
    renderMetadata(node);
    text("diagnostics-json", JSON.stringify(cluster, null, 2));
    text("footer-version", node ? `matchengine-raft ${node.version}` : "matchengine-raft");
  }

  function renderRaft(node) {
    if (!node) return;
    const raft = node.raft;
    const leadership = node.leadership;
    setBadge("raft-health", leadership.is_leader ? "leader" : "follower view", leadership.is_leader ? "leader" : "neutral");
    text("raft-role", leadership.state);
    text("raft-term-vote", `${leadership.term} / ${leadership.vote}`);
    text("raft-last-log", raft.last_log_index);
    text("raft-local-committed", raft.local_committed_index);
    text("raft-cluster-committed", raft.cluster_committed_index);
    text("raft-applied", raft.last_applied_index);
    text("raft-snapshot", raft.snapshot_index);
    text("raft-purged", raft.purged_index);
    const transport = node.transport || {};
    text(
      "transport-heartbeat",
      `${fmt(transport.heartbeat_streams)} / ${fmt(transport.heartbeat_requests)}`,
    );
    text(
      "transport-replication",
      `${fmt(transport.replication_streams)} / ${fmt(transport.replication_requests)}`,
    );
    text(
      "transport-snapshot-sent",
      transport.snapshot_sent
        ? `${transport.snapshot_sent} · ${fmtAge(transport.snapshot_sent_avg_micros / 1000)} avg`
        : "—",
    );
    text(
      "transport-snapshot-received",
      transport.snapshot_received
        ? `${transport.snapshot_received} · ${fmtAge(transport.snapshot_received_avg_micros / 1000)} avg`
        : "—",
    );
    text("raft-voters", `voters ${raft.voters.length ? raft.voters.join(", ") : "—"}`);
    text("raft-learners", `learners ${raft.learners.length ? raft.learners.join(", ") : "—"}`);
    setBadge("replication-scope", leadership.is_leader ? "leader only" : "leader view unavailable", leadership.is_leader ? "leader" : "neutral");

    const body = $("replication-body");
    clear(body);
    const replicas = (raft.replication || []).filter(
      (replica) => replica.node_id !== node.node_id,
    );
    if (replicas.length === 0) {
      const row = document.createElement("tr");
      cell(row, leadership.is_leader ? "No replication targets" : "Open the leader console for replication data", "muted");
      row.firstChild.colSpan = 4;
      body.appendChild(row);
      return;
    }
    for (const replica of replicas) {
      const row = document.createElement("tr");
      cell(row, `node-${replica.node_id}`, "mono");
      cell(row, replica.matched_index, "mono");
      cell(row, replica.lag, `mono ${replica.lag ? "warn" : "good"}`.trim());
      cell(row, fmtAge(replica.last_ack_ms_ago), "mono muted");
      body.appendChild(row);
    }
  }

  function renderStorage(node) {
    if (!node) return;
    const storage = node.storage || {};
    const wal = storage.wal || {};
    const stateMachine = storage.state_machine || {};
    text("storage-wal-last", wal.last_log_id ? wal.last_log_id.index : null);
    text("storage-wal-purged", wal.last_purged_log_id ? wal.last_purged_log_id.index : null);
    text("storage-applied", stateMachine.last_applied ? stateMachine.last_applied.index : null);
    text("storage-snapshot", storage.snapshot ? storage.snapshot.index : null);
    text("storage-sequence", node.orderbook.sequence);
    text("storage-spread", node.orderbook.spread);
    text("storage-last-trade", node.orderbook.last_trade_price);
    text("storage-levels", `${node.orderbook.bid_levels} bid / ${node.orderbook.ask_levels} ask`);
    setBadge("storage-health", node.healthy ? "healthy" : "degraded", node.healthy ? "good" : "bad");

    const events = node.events || {};
    const retained = events.retained_events || 0;
    const limit = events.retention_limit || 0;
    const percent = limit > 0 ? Math.min(100, (retained / limit) * 100) : 0;
    const fill = $("event-fill");
    if (fill) fill.value = percent;
    text("event-label", `${retained} / ${limit}`);
    text("event-oldest", eventId(events.oldest_event_id));
    text("event-last", eventId(events.last_event_id));
    text("event-evictions", events.evictions_total);

    const feed = node.market_data;
    if (!feed) {
      setBadge("feed-badge", "feed off", "neutral");
      text("feed-state", "Market data feed is disabled");
      text("feed-last-seq", "—");
      text("feed-queue", "—");
    } else {
      setBadge("feed-badge", feed.publishing ? "publishing" : "paused", feed.publishing ? "good" : "warn");
      text("feed-state", feed.publishing ? "Market data publishing" : "Market data gate closed");
      text("feed-last-seq", feed.last_market_seq);
      text("feed-queue", `queue ${feed.queued_frames}/${feed.queue_capacity} · gaps ${feed.gaps}`);
    }
  }

  function renderActions(node, cluster) {
    if (!node) return;
    const leaderId = cluster.leader_id;
    const isLeader = node.leadership.is_leader;
    setBadge("action-leader-badge", isLeader ? "leader" : "not leader", isLeader ? "leader" : "warn");
    text(
      "action-hint",
      `Current leader ${leaderId === null ? "—" : `node-${leaderId}`}, term ${cluster.term}. The console polls until a new leader is observed; it does not retry automatically.`,
    );

    const target = $("transfer-target");
    const leader = leaderSnapshot(cluster);
    const candidates = (node.raft.voters || []).filter((id) => id !== node.node_id);
    const desired = candidates.map((id) => {
      const member = cluster.nodes.find((entry) => entry.node_id === id);
      const lag =
        leader && leader.raft && Array.isArray(leader.raft.replication)
          ? leader.raft.replication.find((entry) => entry.node_id === id)?.lag
          : null;
      const reachable = Boolean(member && member.reachable);
      return {
        id: String(id),
        label: `node-${id} — ${reachable ? `lag ${fmt(lag)}` : "unreachable"}`,
        disabled: !reachable || lag !== 0,
      };
    });
    if (desired.length === 0) {
      desired.push({ id: "", label: "No other voters", disabled: true });
    }

    // Rebuild only when the option set changes; otherwise polling would reset
    // the operator's selection and focus every two seconds.
    const signature = JSON.stringify(desired);
    if (target.dataset.signature !== signature) {
      const previous = target.value;
      clear(target);
      for (const item of desired) {
        const option = document.createElement("option");
        option.value = item.id;
        option.textContent = item.label;
        option.disabled = item.disabled;
        target.appendChild(option);
      }
      target.dataset.signature = signature;
      if (previous && desired.some((item) => item.id === previous && !item.disabled)) {
        target.value = previous;
      }
    }

    const confirm = $("transfer-confirm");
    const button = $("transfer-button");
    const updateTransfer = () => {
      button.disabled = !isLeader || !target.value || !confirm.checked;
    };
    confirm.onchange = updateTransfer;
    target.onchange = updateTransfer;
    updateTransfer();

    const elect = $("elect-button");
    elect.disabled = !(node.raft.voters || []).includes(node.node_id);
    $("snapshot-button").disabled = false;

    const purgeConfirm = $("purge-confirm");
    const purgeButton = $("purge-button");
    const updatePurge = () => {
      purgeButton.disabled = !purgeConfirm.checked;
    };
    purgeConfirm.onchange = updatePurge;
    updatePurge();

    const runtimeConfirm = $("runtime-confirm");
    const runtimeButton = $("runtime-button");
    const updateRuntime = () => {
      runtimeButton.disabled = !runtimeConfirm.checked || isLeader;
    };
    runtimeConfirm.onchange = updateRuntime;
    updateRuntime();
  }

  function renderMetadata(node) {
    if (!node) return;
    text("meta-version", node.version);
    text("meta-started", fmtTime(node.started_at_ms));
    text("meta-http", node.http_addr);
    text("meta-grpc", node.grpc_addr);
    text("meta-capabilities", (node.capabilities || []).join(", "));
  }

  function eventId(id) {
    return id ? `${id.log_index}.${id.ordinal}` : "—";
  }

  function renderActionsHistory(actions) {
    state.actions = actions || [];
    text("action-count", `last ${state.actions.length}`);
    const list = $("action-history");
    clear(list);
    if (state.actions.length === 0) {
      const item = document.createElement("li");
      const span = document.createElement("span");
      span.className = "muted";
      span.textContent = "No actions yet";
      item.appendChild(span);
      list.appendChild(item);
      return;
    }
    for (const action of state.actions) {
      const item = document.createElement("li");
      const time = document.createElement("time");
      time.textContent = new Date(action.at_ms).toLocaleTimeString();
      const span = document.createElement("span");
      span.textContent = `${action.action}${action.target ? ` node-${action.target}` : ""} · ${action.result}${
        action.error ? ` · ${action.error}` : ""
      }`;
      item.appendChild(time);
      item.appendChild(span);
      list.appendChild(item);
    }
  }

  async function refreshActions() {
    try {
      renderActionsHistory(await fetchJSON("/console/api/actions"));
    } catch (_) {
      // The cluster view is still useful if action history is unavailable.
    }
  }

  function renderEvents(response) {
    const page = response && response.Ok ? response.Ok : response;
    const events = (page && page.events) || [];
    state.events = events;
    state.eventsAfter = page ? page.next_after : null;
    const kind = $("event-kind").value;
    const side = $("event-side").value;
    const visible = events.filter(
      (event) => (!kind || event.kind === kind) && (!side || event.side === side),
    );
    const body = $("events-body");
    clear(body);
    for (const event of visible) {
      const row = document.createElement("tr");
      cell(row, eventId(event.event_id), "mono");
      cell(row, event.kind, `badge ${event.kind === "fill" ? "good" : event.kind === "cancelled" ? "warn" : "neutral"}`);
      cell(row, event.side);
      cell(row, event.price, "mono");
      cell(row, event.volume, "mono");
      cell(row, event.order_sequence, "mono");
      body.appendChild(row);
    }
    const status = $("events-status");
    if (response && response.Err) {
      status.textContent = "Leader-only: open the leader console for events";
    } else if (page) {
      status.textContent = `Showing ${visible.length} of ${events.length} on this page`;
    } else {
      status.textContent = "No events";
    }
    $("events-older").disabled = !state.eventsAfter;
  }

  async function refreshEvents(reset = false) {
    const local = state.node;
    if (!local) return;
    if (!local.leadership.is_leader) {
      renderEvents({ Err: "leader only" });
      return;
    }
    if (reset) state.eventsAfter = null;
    try {
      const response = await fetchJSON("/events", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          after: state.eventsAfter,
          limit: Number($("event-limit").value),
        }),
      });
      renderEvents(response);
    } catch (error) {
      const status = $("events-status");
      if (status) status.textContent = `Event fetch failed: ${error.message}`;
    }
  }

  async function refresh() {
    if (state.loading) return;
    state.loading = true;
    try {
      const cluster = await fetchJSON("/console/api/cluster");
      renderCluster(cluster);
      await refreshActions();
      if ($("tab-events").checked && !state.eventsPaused) await refreshEvents(true);
    } catch (error) {
      setChip("cluster-chip", `Console unavailable: ${error.message}`, "bad");
      text("updated", `Stale · ${error.message}`);
    } finally {
      state.loading = false;
    }
  }

  async function postAction(path, body) {
    const result = $("action-result");
    if (result) result.textContent = "Submitting…";
    try {
      const response = await fetchJSON(path, {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "X-Console-Action": "1",
        },
        body: JSON.stringify(body || {}),
      });
      if (result) result.textContent = response.message || "accepted";
    } catch (error) {
      if (result) result.textContent = error.message;
    }
    await refresh();
  }

  function schedule() {
    if (state.timer) clearInterval(state.timer);
    state.timer = null;
    if ($("auto-refresh").checked && !document.hidden) {
      state.timer = setInterval(refresh, 2000);
    }
  }

  $("refresh-button").addEventListener("click", refresh);
  $("auto-refresh").addEventListener("change", schedule);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) refresh();
    schedule();
  });
  $("transfer-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const target = Number($("transfer-target").value);
    if (!target) return;
    postAction("/console/api/actions/transfer-leader", { target });
  });
  $("elect-button").addEventListener("click", () => postAction("/console/api/actions/elect", {}));
  $("snapshot-button").addEventListener("click", () => postAction("/console/api/actions/snapshot", {}));
  $("purge-form").addEventListener("submit", (event) => {
    event.preventDefault();
    if (!$("purge-confirm").checked) return;
    const raw = $("purge-upto").value;
    postAction("/console/api/actions/purge-log", raw ? { upto: Number(raw) } : {});
  });
  $("runtime-form").addEventListener("submit", (event) => {
    event.preventDefault();
    if (!$("runtime-confirm").checked) return;
    const body = {};
    for (const [field, id] of [
      ["tick", "runtime-tick"],
      ["heartbeat", "runtime-heartbeat"],
      ["elect", "runtime-elect"],
      ["pre_vote", "runtime-pre-vote"],
    ]) {
      const value = $(id).value;
      if (value !== "") body[field] = value === "true";
    }
    postAction("/console/api/actions/runtime-config", body);
  });
  $("events-pause").addEventListener("click", () => {
    state.eventsPaused = !state.eventsPaused;
    $("events-pause").textContent = state.eventsPaused ? "Resume" : "Pause";
    if (!state.eventsPaused) refreshEvents(false);
  });
  $("events-newer").addEventListener("click", () => refreshEvents(true));
  $("events-older").addEventListener("click", () => refreshEvents(false));
  $("event-kind").addEventListener("change", () => renderEvents({ Ok: { events: state.events, next_after: state.eventsAfter } }));
  $("event-side").addEventListener("change", () => renderEvents({ Ok: { events: state.events, next_after: state.eventsAfter } }));
  $("tab-events").addEventListener("change", () => {
    if ($("tab-events").checked) refreshEvents(true);
  });

  $("copy-json").addEventListener("click", async () => {
    const payload = JSON.stringify(state.cluster, null, 2);
    try {
      await navigator.clipboard.writeText(payload);
      $("copy-json").textContent = "Copied";
    } catch (_) {
      $("copy-json").textContent = "Copy failed";
    }
  });
  $("download-json").addEventListener("click", () => {
    const payload = JSON.stringify(state.cluster, null, 2);
    const blob = new Blob([payload], { type: "application/json" });
    const link = document.createElement("a");
    link.href = URL.createObjectURL(blob);
    link.download = `matchengine-cluster-${Date.now()}.json`;
    link.click();
    URL.revokeObjectURL(link.href);
  });

  refresh();
  schedule();
})();
