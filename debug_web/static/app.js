let ws = null;
let lastSnapshot = null;
let lastRender = 0;

const statusEl = document.getElementById("status");
const summaryEl = document.getElementById("summary");
const rowsEl = document.getElementById("rows");
const filterEl = document.getElementById("filter");

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
      lastSnapshot = JSON.parse(ev.data);
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
  if (!lastSnapshot) return;
  const s = lastSnapshot;
  const filter = (filterEl.value || "").toLowerCase().trim();

  summaryEl.textContent =
    `${s.market_count} markets | both=${s.with_both_prices} | ` +
    `Δk=${s.kalshi_delta} Δp=${s.polymarket_delta} | ` +
    `k=${s.kalshi_total_updates} p=${s.polymarket_total_updates}`;

  const markets = (s.markets || [])
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

    const age = `${fmtAge(s.now_ms, m.k_last_ms)} / ${fmtAge(s.now_ms, m.p_last_ms)}`;
    const ageMax = Math.max(
      m.k_last_ms ? (s.now_ms - m.k_last_ms) : 0,
      m.p_last_ms ? (s.now_ms - m.p_last_ms) : 0
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

