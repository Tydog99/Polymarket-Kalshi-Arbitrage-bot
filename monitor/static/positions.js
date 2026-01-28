function fmtNum(n) {
  if (n === null || n === undefined) return "—";
  const x = Number(n);
  if (!Number.isFinite(x)) return "—";
  return x % 1 === 0 ? String(x) : x.toFixed(2);
}

function fmtMoney(n) {
  if (n === null || n === undefined) return "—";
  const x = Number(n);
  if (!Number.isFinite(x)) return "—";
  return x.toFixed(2);
}

function fmtPct(n) {
  if (n === null || n === undefined) return "—";
  const x = Number(n);
  if (!Number.isFinite(x)) return "—";
  return `${x.toFixed(2)}%`;
}

function safe(obj, path, fallback = null) {
  try {
    return path.split(".").reduce((acc, k) => acc?.[k], obj) ?? fallback;
  } catch {
    return fallback;
  }
}

function computeExpected(p) {
  // "Capital" = sum of all leg cost bases + fees (dollars)
  const kyC = Number(safe(p, "kalshi_yes.contracts", 0)) || 0;
  const knC = Number(safe(p, "kalshi_no.contracts", 0)) || 0;
  const pyC = Number(safe(p, "poly_yes.contracts", 0)) || 0;
  const pnC = Number(safe(p, "poly_no.contracts", 0)) || 0;

  const kyCost = Number(safe(p, "kalshi_yes.cost_basis", 0)) || 0;
  const knCost = Number(safe(p, "kalshi_no.cost_basis", 0)) || 0;
  const pyCost = Number(safe(p, "poly_yes.cost_basis", 0)) || 0;
  const pnCost = Number(safe(p, "poly_no.cost_basis", 0)) || 0;
  const fees = Number(p?.total_fees ?? 0) || 0;

  const capital = kyCost + knCost + pyCost + pnCost + fees;

  // "Expected payout" assumes you hold both YES and NO on the same underlying event.
  // Regardless of outcome, only one side pays out $1, so payout equals the paired contracts.
  const yesTotal = kyC + pyC;
  const noTotal = knC + pnC;
  const paired = Math.min(yesTotal, noTotal);
  const expectedPayout = paired * 1.0;

  const expectedPnl = expectedPayout - capital;
  const roiPct = capital > 0 ? (expectedPnl / capital) * 100.0 : null;

  return { capital, expectedPayout, expectedPnl, roiPct, paired, yesTotal, noTotal };
}

function renderPositions(payload) {
  const body = document.getElementById("positionsBody");
  const positions = payload?.positions || {};
  const entries = Object.values(positions);

  // newest first when we can
  entries.sort((a, b) => {
    const ta = Date.parse(a?.opened_at || "") || 0;
    const tb = Date.parse(b?.opened_at || "") || 0;
    return tb - ta;
  });

  if (entries.length === 0) {
    body.innerHTML =
      '<tr><td colspan="11" class="empty">No positions found.</td></tr>';
    return;
  }

  body.innerHTML = entries
    .map((p) => {
      const desc = p.description || p.market_id || "—";
      const opened = p.opened_at ? new Date(p.opened_at).toLocaleString() : "—";
      const status = (p.status || "unknown").toLowerCase();
      const statusClass = status === "open" ? "status-open" : "status-closed";
      const ky = safe(p, "kalshi_yes.contracts", 0);
      const kn = safe(p, "kalshi_no.contracts", 0);
      const py = safe(p, "poly_yes.contracts", 0);
      const pn = safe(p, "poly_no.contracts", 0);
      const fees = p.total_fees ?? null;

      const exp = computeExpected(p);
      const pnlClass =
        exp.expectedPnl >= 0 ? "goodText" : "badText";
      const roiClass =
        exp.roiPct != null && exp.roiPct >= 0 ? "goodText" : "badText";

      return `
        <tr>
          <td>
            <div>${desc}</div>
            <div class="muted mono" style="margin-top:4px">${p.market_id || ""}</div>
          </td>
          <td class="muted">${opened}</td>
          <td class="${statusClass}">${status}</td>
          <td class="right mono">${fmtNum(ky)}</td>
          <td class="right mono">${fmtNum(kn)}</td>
          <td class="right mono">${fmtNum(py)}</td>
          <td class="right mono">${fmtNum(pn)}</td>
          <td class="right mono">${fmtMoney(fees)}</td>
          <td class="right mono">${fmtMoney(exp.capital)}</td>
          <td class="right mono ${pnlClass}">${fmtMoney(exp.expectedPnl)}</td>
          <td class="right mono ${roiClass}">${fmtPct(exp.roiPct)}</td>
        </tr>
      `;
    })
    .join("");
}

function setConn(ok, text) {
  const dot = document.getElementById("connDot");
  const t = document.getElementById("connText");
  dot.classList.toggle("good", !!ok);
  t.textContent = text;
}

async function fetchSnapshot() {
  const res = await fetch("/api/positions", { cache: "no-store" });
  const json = await res.json();
  renderPositions(json);
}

function startStream() {
  const es = new EventSource("/api/positions/stream");

  es.addEventListener("open", () => {
    setConn(true, "Live");
  });

  es.addEventListener("error", () => {
    setConn(false, "Disconnected (retrying)");
  });

  es.addEventListener("positions", (msg) => {
    try {
      const payload = JSON.parse(msg.data);
      renderPositions(payload);
      setConn(true, "Live");
    } catch {
      // ignore parse errors; keep last render
    }
  });
}

window.addEventListener("DOMContentLoaded", async () => {
  try {
    await fetchSnapshot();
  } catch {
    // ignore; stream will fill
  }
  startStream();
});

