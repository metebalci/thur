/*
 * Thur Web UI — read-only dashboard logic.
 *
 * No framework, no build. Detect the product from /info, then poll the
 * read-only /api/v1 surface every REFRESH_MS and repaint. Every request
 * rides the browser's HTTP Basic session (the same `webadmin` password
 * the listener gates on), so there is no login form here — the browser
 * prompts on the first 401. Mutations are deliberately absent (#91).
 */

"use strict";

const REFRESH_MS = 5000;

/** Current product slug ("thurvtl" | "thurvsa"), set after /info. */
let PRODUCT = null;
/** Wall-clock skew helper: daemon start epoch (seconds) from monitor. */
let timer = null;

// ---- tiny DOM + format helpers -------------------------------------------

const $ = (id) => document.getElementById(id);

function el(tag, attrs, children) {
  const node = document.createElement(tag);
  if (attrs) {
    for (const [k, v] of Object.entries(attrs)) {
      if (k === "class") node.className = v;
      else if (k === "html") node.innerHTML = v;
      else node.setAttribute(k, v);
    }
  }
  for (const c of children || []) {
    node.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return node;
}

function clear(node) {
  while (node.firstChild) node.removeChild(node.firstChild);
}

function bytes(n) {
  n = Number(n) || 0;
  if (n < 1024) return `${n} B`;
  const units = ["KiB", "MiB", "GiB", "TiB", "PiB"];
  let i = -1;
  do {
    n /= 1024;
    i++;
  } while (n >= 1024 && i < units.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${units[i]}`;
}

function num(n) {
  return (Number(n) || 0).toLocaleString();
}

function duration(secs) {
  secs = Math.max(0, Math.floor(secs));
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${secs % 60}s`;
  return `${secs}s`;
}

function ago(iso) {
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "";
  const s = Math.floor((Date.now() - t) / 1000);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function hhmmss(iso) {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return "";
  return d.toLocaleTimeString([], { hour12: false });
}

// ---- fetch ---------------------------------------------------------------

async function api(path) {
  const resp = await fetch(path, {
    credentials: "same-origin",
    headers: { Accept: "application/json" },
  });
  if (!resp.ok) {
    const err = new Error(`${path}: ${resp.status}`);
    err.status = resp.status;
    throw err;
  }
  return resp.json();
}

function setStatus(state, text) {
  const s = $("status");
  s.dataset.state = state;
  s.querySelector(".status-text").textContent = text;
}

// ---- KPI cards -----------------------------------------------------------

function kpi(label, value, sub, meter) {
  const card = el("div", { class: "kpi" }, [
    el("div", { class: "kpi-label" }, [label]),
    el("div", { class: "kpi-value" }, [value]),
  ]);
  if (sub) card.append(el("div", { class: "kpi-sub" }, [sub]));
  if (meter) {
    const fill = el("span");
    fill.style.width = `${Math.min(100, Math.max(0, meter.pct))}%`;
    const bar = el("div", { class: `meter ${meter.cls || ""}` }, [fill]);
    card.append(bar);
  }
  return card;
}

function renderKpis(mon) {
  const wrap = $("kpis");
  clear(wrap);
  const p = mon.product || {};

  if (PRODUCT === "thurvtl") {
    wrap.append(
      kpi("Cartridges", num(p.cartridges_total), `${num(p.cartridges_loaded)} loaded`),
    );
    wrap.append(
      kpi("Drives busy", `${num(p.drives_busy)} / ${num(p.drives_total)}`, "in use / total"),
    );
  } else {
    wrap.append(kpi("Volumes online", num(p.volumes_online), "attached"));
  }
  wrap.append(kpi("Sessions", num(p.sessions_active), "active iSCSI / NVMe"));

  // Pool fill: sum the per-backend global rows (namespace === null).
  const pool = (mon.pool || []).filter((r) => r.namespace == null);
  const used = pool.reduce((a, r) => a + (r.used_bytes || 0), 0);
  const cap = pool.reduce((a, r) => a + (r.cap_bytes || 0), 0);
  const pct = cap > 0 ? (used / cap) * 100 : 0;
  const cls = pct >= 90 ? "err" : pct >= 75 ? "warn" : "";
  wrap.append(
    kpi(
      "Pool cache",
      bytes(used),
      cap > 0 ? `of ${bytes(cap)} (${pct.toFixed(0)}%)` : "no cap",
      cap > 0 ? { pct, cls } : null,
    ),
  );

  const audit = (mon.audit && mon.audit.entries_total) || 0;
  wrap.append(kpi("Audit entries", num(audit), "since boot"));
}

// ---- inventory: VTL ------------------------------------------------------

function table(headers, rows) {
  const thead = el("thead", null, [
    el("tr", null, headers.map((h) => el("th", h.cls ? { class: h.cls } : null, [h.label || h]))),
  ]);
  const tbody = el("tbody", null, rows);
  return el("table", null, [thead, tbody]);
}

function renderVtl(data) {
  const { cartridges, drives } = data;

  // Cartridges
  const cs = (cartridges && cartridges.cartridges) || [];
  $("inventory-title").textContent = "Cartridges";
  $("inventory-count").textContent = `${cs.length} total`;
  const invBody = $("inventory-body");
  clear(invBody);
  if (cs.length === 0) {
    invBody.append(el("p", { class: "empty" }, ["No cartridges in the library."]));
  } else {
    const rows = cs.map((c) =>
      el("tr", null, [
        el("td", { class: "mono" }, [c.barcode]),
        el("td", null, [el("span", { class: "badge" }, [c.location])]),
        el("td", { class: "num" }, [c.slot_id]),
      ]),
    );
    invBody.append(table([{ label: "Barcode" }, { label: "Location" }, { label: "Slot", cls: "num" }], rows));
  }

  // Drives
  const ds = (drives && drives.drives) || [];
  $("secondary-panel").hidden = false;
  $("secondary-title").textContent = "Drives";
  $("secondary-count").textContent = `${ds.length} total`;
  const secBody = $("secondary-body");
  clear(secBody);
  if (ds.length === 0) {
    secBody.append(el("p", { class: "empty" }, ["No drives configured."]));
  } else {
    const rows = ds.map((d) =>
      el("tr", null, [
        el("td", { class: "num" }, [d.id]),
        el("td", null, [
          d.loaded
            ? el("span", { class: "badge accent" }, ["loaded"])
            : el("span", { class: "badge" }, ["empty"]),
        ]),
        el("td", { class: "mono" }, [d.barcode || "—"]),
        el("td", { class: "num" }, [d.total_blocks == null ? "—" : num(d.total_blocks)]),
      ]),
    );
    secBody.append(
      table(
        [{ label: "Drive", cls: "num" }, { label: "State" }, { label: "Cartridge" }, { label: "Blocks", cls: "num" }],
        rows,
      ),
    );
  }
}

// ---- inventory: VSA ------------------------------------------------------

function renderVsa(data) {
  const vols = (data.volumes && data.volumes.volumes) || [];
  $("inventory-title").textContent = "Volumes";
  $("inventory-count").textContent = `${vols.length} total`;
  $("secondary-panel").hidden = true;
  const invBody = $("inventory-body");
  clear(invBody);
  if (vols.length === 0) {
    invBody.append(el("p", { class: "empty" }, ["No volumes created."]));
    return;
  }
  const rows = vols.map((v) =>
    el("tr", null, [
      el("td", null, [v.name]),
      el("td", { class: "num" }, [v.lun]),
      el("td", { class: "num" }, [bytes(v.size_bytes)]),
      el("td", { class: "mono dim" }, [v.backend]),
      el("td", null, [
        ...(v.worm ? [el("span", { class: "badge warn" }, ["WORM"])] : []),
        el("span", { class: "badge" }, [v.dedup_scope]),
      ]),
    ]),
  );
  invBody.append(
    table(
      [{ label: "Name" }, { label: "LUN", cls: "num" }, { label: "Size", cls: "num" }, { label: "Backend" }, { label: "Flags" }],
      rows,
    ),
  );
}

// ---- jobs + audit --------------------------------------------------------

function renderJobs(data) {
  const jobs = (data && data.jobs) || [];
  $("jobs-count").textContent = jobs.length ? `${jobs.length}` : "";
  const body = $("jobs-body");
  clear(body);
  if (jobs.length === 0) {
    body.append(el("p", { class: "empty" }, ["No jobs in the last 5 minutes."]));
    return;
  }
  const rows = jobs.map((j) => {
    let badge;
    if (!j.finished) badge = el("span", { class: "badge accent" }, ["running"]);
    else if (j.exit_code === 0) badge = el("span", { class: "badge ok" }, ["ok"]);
    else badge = el("span", { class: "badge err" }, [`exit ${j.exit_code}`]);
    return el("tr", null, [
      el("td", { class: "mono dim" }, [j.id]),
      el("td", { class: "mono" }, [j.kind]),
      el("td", null, [badge]),
      el("td", { class: "num dim" }, [ago(j.started_at)]),
    ]);
  });
  body.append(
    table([{ label: "Id" }, { label: "Kind" }, { label: "State" }, { label: "Started", cls: "num" }], rows),
  );
}

function renderAudit(data) {
  const entries = (data && data.entries) || [];
  $("audit-count").textContent = entries.length ? `last ${entries.length}` : "";
  const body = $("audit-body");
  clear(body);
  if (entries.length === 0) {
    body.append(el("p", { class: "empty" }, ["No audit entries."]));
    return;
  }
  // Newest first.
  for (const e of entries.slice().reverse()) {
    const failed = e.result && e.result !== "ok";
    const actor = e.actor || {};
    const who = actor.user || actor.kind || "";
    body.append(
      el("div", { class: "row" }, [
        el("span", { class: "ts" }, [hhmmss(e.ts)]),
        el("span", { class: "op" }, [
          el("span", failed ? { class: "badge err" } : { class: "badge" }, [e.op]),
        ]),
        el("span", { class: "actor" }, [who]),
      ]),
    );
  }
}

// ---- refresh loop --------------------------------------------------------

async function refresh() {
  try {
    const mon = await api("/api/v1/monitor");
    renderKpis(mon);
    if (mon.started_at_unix && mon.ts_unix) {
      $("uptime").textContent = `up ${duration(mon.ts_unix - mon.started_at_unix)}`;
    }
    $("version").textContent = mon.version || "";

    if (PRODUCT === "thurvtl") {
      const [cartridges, drives] = await Promise.all([
        api("/api/v1/cartridges").catch(() => ({})),
        api("/api/v1/drives").catch(() => ({})),
      ]);
      renderVtl({ cartridges, drives });
    } else {
      const volumes = await api("/api/v1/volumes").catch(() => ({}));
      renderVsa({ volumes });
    }

    const [jobs, audit] = await Promise.all([
      api("/api/v1/jobs/recent").catch(() => ({ jobs: [] })),
      api("/api/v1/audit/tail?lines=50").catch(() => ({ entries: [] })),
    ]);
    renderJobs(jobs);
    renderAudit(audit);

    setStatus("ok", "live");
    $("updated").textContent = `updated ${new Date().toLocaleTimeString([], { hour12: false })}`;
  } catch (e) {
    setStatus("error", e.status ? `error ${e.status}` : "unreachable");
  }
}

// ---- bootstrap -----------------------------------------------------------

async function init() {
  try {
    const info = await api("/info");
    PRODUCT = info.product === "thurvtl" ? "thurvtl" : "thurvsa";
    document.documentElement.dataset.product = PRODUCT;

    const label = PRODUCT === "thurvtl" ? "Thur VTL" : "Thur VSA";
    const kind = PRODUCT === "thurvtl" ? "tape library" : "storage array";
    $("product-name").textContent = label;
    $("product-kind").textContent = kind;
    document.title = `${label} — admin`;

    $("splash").hidden = true;
    $("layout").hidden = false;

    await refresh();
    timer = setInterval(refresh, REFRESH_MS);
  } catch (e) {
    const card = document.querySelector(".splash-card");
    card.classList.add("error");
    if (e.status === 401) {
      $("splash-title").textContent = "Authentication required";
      $("splash-detail").innerHTML =
        "Reload and sign in with the <code>webadmin</code> password (<code>system set-admin-password</code>).";
    } else if (e.status === 503) {
      $("splash-title").textContent = "No admin password set";
      $("splash-detail").innerHTML =
        "Set one with <code>system set-admin-password</code>, then reload.";
    } else {
      $("splash-title").textContent = "Cannot reach the appliance";
      $("splash-detail").textContent = String(e.message || e);
    }
    setStatus("error", "offline");
  }
}

document.addEventListener("DOMContentLoaded", init);
