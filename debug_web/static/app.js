let ws = null;
let lastRender = 0;

const statusEl = document.getElementById("status");
const summaryEl = document.getElementById("summary");
const rowsEl = document.getElementById("rows");
const filterEl = document.getElementById("filter");

const marketsById = new Map();
let meta = {
  now_ms: 0,
  threshold_cents: 0,
  market_count: 0,
  with_both_prices: 0,
  kalshi_total_updates: 0,
  polymarket_total_updates: 0,
  kalshi_delta: 0,
  polymarket_delta: 0,
};

function connect() {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const url = `${proto}://${location.host}/ws`;
  ws = new WebSocket(url);

  ws.onopen = () => {
    statusEl.textContent = "connected";
    statusEl.className = "pill good";
  };

  ws.onclose = () => {
    statusEl.textContent = "disconnected (reconnecting)";
    statusEl.className = "pill warn";
    setTimeout(connect, 500);
  };

  ws.onerror = () => {
    statusEl.textContent = "socket error";
    statusEl.className = "pill bad";
  };

  ws.onmessage = (ev) => {
    try {
      const msg = JSON.parse(ev.data);
      if (msg && msg.type === "snapshot") {
        meta = { ...meta, ...msg };
        marketsById.clear();
        for (const m of msg.markets || []) {
          if (m && m.market_id != null) marketsById.set(m.market_id, m);
        }
      } else if (msg && msg.type === "market_update" && msg.market) {
        meta.now_ms = msg.now_ms || meta.now_ms;
        meta.threshold_cents = msg.threshold_cents || meta.threshold_cents;
        const m = msg.market;
        if (m.market_id != null) marketsById.set(m.market_id, m);
      } else {
        // Back-compat: treat untyped messages as a snapshot
        if (msg && msg.markets) {
          meta = { ...meta, ...msg };
          marketsById.clear();
          for (const m of msg.markets || []) {
            if (m && m.market_id != null) marketsById.set(m.market_id, m);
          }
        }
      }
      maybeRender();
    } catch (e) {
      // ignore malformed frames
    }
  };
}

function fmtPricePair(yes, no) {
  if (yes > 0 && no > 0) return `${String(yes).padStart(2, "0")}/${String(no).padStart(2, "0")}`;
  return "--/--";
}

function fmtAge(nowMs, lastMs) {
  if (!lastMs) return "--";
  const d = Math.max(0, nowMs - lastMs);
  if (d < 1000) return `${d}ms`;
  if (d < 60000) return `${Math.round(d / 1000)}s`;
  return `${Math.round(d / 60000)}m`;
}

function render() {
  const s = meta;
  const filter = (filterEl.value || "").toLowerCase().trim();

  const marketsArr = Array.from(marketsById.values());
  const marketCount = marketsArr.length;
  const withBoth = marketsArr.reduce((acc, m) => acc + ((m.k_yes > 0 && m.k_no > 0 && m.p_yes > 0 && m.p_no > 0) ? 1 : 0), 0);

  summaryEl.textContent =
    `${marketCount} markets | both=${withBoth} | ` +
    `Δk=${s.kalshi_delta || 0} Δp=${s.polymarket_delta || 0} | ` +
    `k=${s.kalshi_total_updates || 0} p=${s.polymarket_total_updates || 0}`;

  const markets = marketsArr
    .filter((m) => {
      if (!filter) return true;
      return (m.description || "").toLowerCase().includes(filter) ||
        (m.league || "").toLowerCase().includes(filter) ||
        (m.market_type || "").toLowerCase().includes(filter);
    })
    .sort((a, b) => {
      // Best (most negative) gaps first; then freshest.
      const ag = a.gap_cents == null ? 999999 : a.gap_cents;
      const bg = b.gap_cents == null ? 999999 : b.gap_cents;
      if (ag !== bg) return ag - bg;
      const aa = Math.max(a.k_last_ms || 0, a.p_last_ms || 0);
      const ba = Math.max(b.k_last_ms || 0, b.p_last_ms || 0);
      return ba - aa;
    });

  const fr = document.createDocumentFragment();
  for (const m of markets) {
    const tr = document.createElement("tr");

    const gap = m.gap_cents == null ? "--" : `${m.gap_cents}c`;
    const gapClass = m.gap_cents == null ? "muted" : (m.gap_cents <= 0 ? "good mono" : "mono");

    const k = fmtPricePair(m.k_yes, m.k_no);
    const p = fmtPricePair(m.p_yes, m.p_no);
    const size = `${m.yes_size || 0}/${m.no_size || 0}`;
    const upd = `${m.k_updates || 0}/${m.p_updates || 0}`;

    const nowMs = s.now_ms || Date.now();
    const age = `${fmtAge(nowMs, m.k_last_ms)} / ${fmtAge(nowMs, m.p_last_ms)}`;
    const ageMax = Math.max(
      m.k_last_ms ? (nowMs - m.k_last_ms) : 0,
      m.p_last_ms ? (nowMs - m.p_last_ms) : 0
    );
    const ageClass = ageMax > 30000 ? "warn mono" : "mono";

    tr.innerHTML = `
      <td class="mono">${(m.league || "").toUpperCase()}</td>
      <td class="mono">${m.market_type || ""}</td>
      <td>${m.description || ""}</td>
      <td class="mono">${k}</td>
      <td class="mono">${p}</td>
      <td class="${gapClass}">${gap}</td>
      <td class="mono">${size}</td>
      <td class="mono">${upd}</td>
      <td class="${ageClass}">${age}</td>
    `;
    fr.appendChild(tr);
  }
  rowsEl.replaceChildren(fr);
}

function maybeRender() {
  const now = performance.now();
  if (now - lastRender < 200) return; // throttle
  lastRender = now;
  render();
}

filterEl.addEventListener("input", () => render());
connect();

