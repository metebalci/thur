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
// How many trailing audit entries the tail panel shows (it scrolls).
const AUDIT_LINES = 50;

/** Current product slug ("thurvtl" | "thurvsa"), set after /info. */
let PRODUCT = null;
let timer = null;
/** Previous backend byte totals + timestamp, for the per-second
 *  upload/download rate averaged over the gap between polls. */
let prevIo = null;

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

// Bare GiB number (no unit suffix) for KPIs that carry their unit in
// the title, e.g. "Pool cache (GiB)" -> "0 / 7.58".
function gib(n) {
  const v = (Number(n) || 0) / (1024 * 1024 * 1024);
  if (v === 0) return "0";
  return v >= 100 ? v.toFixed(0) : v >= 10 ? v.toFixed(1) : v.toFixed(2);
}

// Byte-rate in MB/s (decimal MB, the throughput convention) for the
// Up/Down KPI — the unit lives in the title so the value stays bare.
function mbps(bps) {
  const v = (Number(bps) || 0) / 1e6;
  if (v === 0) return "0";
  return v >= 10 ? v.toFixed(1) : v.toFixed(2);
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
      kpi("Drives", `${num(p.drives_busy)} / ${num(p.drives_total)}`, "in use / total"),
    );
    wrap.append(
      kpi(
        "Cartridges",
        `${num(p.cartridges_loaded)} / ${num(p.cartridges_total)}`,
        "loaded / total",
      ),
    );
    // Tape is iSCSI-only — no NVMe/TCP transport, so a single count.
    wrap.append(kpi("Sessions", num(p.sessions_active), "iSCSI"));
  } else {
    wrap.append(kpi("Volumes online", num(p.volumes_online), "attached"));
    // VSA speaks both transports; show them split. Fall back to the
    // legacy combined field if an older daemon doesn't send the split.
    const iscsi = p.iscsi_sessions ?? p.sessions_active ?? 0;
    const nvme = p.nvmetcp_sessions ?? 0;
    wrap.append(kpi("Sessions", `${num(iscsi)} / ${num(nvme)}`, "iSCSI / NVMe/TCP"));
  }

  // Pool fill: sum the per-backend global rows (namespace === null).
  const pool = (mon.pool || []).filter((r) => r.namespace == null);
  const used = pool.reduce((a, r) => a + (r.used_bytes || 0), 0);
  const cap = pool.reduce((a, r) => a + (r.cap_bytes || 0), 0);
  const pct = cap > 0 ? (used / cap) * 100 : 0;
  const cls = pct >= 90 ? "err" : pct >= 75 ? "warn" : "";
  wrap.append(
    kpi(
      "Pool cache (GiB)",
      cap > 0 ? `${gib(used)} / ${gib(cap)}` : gib(used),
      cap > 0 ? `used / cap (${pct.toFixed(0)}%)` : "no cap",
      cap > 0 ? { pct, cls } : null,
    ),
  );

  // Backend upload/download, averaged over the gap since the last poll
  // (~5 s). PUT bytes = upload to the backend, GET = download from it.
  const putTotal = (mon.storage || []).reduce((a, s) => a + (s.put_bytes_total || 0), 0);
  const getTotal = (mon.storage || []).reduce((a, s) => a + (s.get_bytes_total || 0), 0);
  const ts = mon.ts_unix || 0;
  let upRate = 0;
  let downRate = 0;
  if (prevIo && ts > prevIo.ts) {
    const dt = ts - prevIo.ts;
    upRate = Math.max(0, (putTotal - prevIo.put) / dt);
    downRate = Math.max(0, (getTotal - prevIo.get) / dt);
  }
  prevIo = { ts, put: putTotal, get: getTotal };
  wrap.append(
    kpi("Throughput (MB/s)", `${mbps(upRate)} / ${mbps(downRate)}`, "upload / download"),
  );
}

// ---- shared table helper -------------------------------------------------

function table(headers, rows, cls) {
  const thead = el("thead", null, [
    el("tr", null, headers.map((h) => el("th", h.cls ? { class: h.cls } : null, [h.label || h]))),
  ]);
  const tbody = el("tbody", null, rows);
  return el("table", cls ? { class: cls } : null, [thead, tbody]);
}

// ---- library map: VTL ----------------------------------------------------

// Above this many storage slots we stop drawing empty cells and show
// occupied slots only — a 65535-slot grid would be unusable. Realistic
// libraries (tens to low hundreds of slots) fall well under this.
const SLOT_CELL_CAP = 1024;

function libSection(text) {
  return el("div", { class: "lib-section" }, [text]);
}

// One slot/drive cell. `idx` is the slot/drive label, `barcode` the
// occupant (or null/empty). Filled cells are accented; the barcode is
// also the hover title so a truncated one is still readable.
function slotCell(idx, barcode, opts) {
  opts = opts || {};
  const filled = barcode != null && barcode !== "";
  // State: filled (solid accent), home (empty, but the return slot of a
  // loaded cartridge — dashed accent outline), or plain empty.
  const state = filled ? "filled" : opts.home ? "empty home" : "empty";
  const c = el(
    "div",
    { class: `cell ${state}${opts.wide ? " wide" : ""}` },
    [el("span", { class: "cell-txt" }, [filled ? barcode : String(idx)])],
  );
  // Hover reveals the position (and any extra detail).
  const title = opts.title || (filled ? `${idx} · ${barcode}` : null);
  if (title) c.setAttribute("title", title);
  return c;
}

function renderLibrary(data) {
  const info = data.info || {};
  const ds = (data.drives && data.drives.drives) || [];
  const cs = (data.cartridges && data.cartridges.cartridges) || [];
  const storageTotal = info.storage_slots || 0;
  const mailTotal = info.mail_slots || 0;

  const storageBySlot = {};
  const mailQueue = [];
  for (const c of cs) {
    if (c.location === "storage") storageBySlot[c.slot_id] = c.barcode;
    else if (c.location === "mail") mailQueue.push(c.barcode);
  }
  const filled = Object.keys(storageBySlot).length;
  // Home slot -> the loaded drive that returns to it. Used to dash-mark
  // the (empty) home slot of a cartridge currently in a drive.
  const homeOfSlot = {};
  for (const d of ds) {
    if (d.barcode && d.home_slot != null) homeOfSlot[d.home_slot] = d;
  }

  $("inventory-title").textContent = "Library";
  $("inventory-count").textContent = `${ds.length} drives · ${storageTotal} slots`;
  const body = $("inventory-body");
  clear(body);

  // Drives row.
  body.append(libSection("Drives"));
  if (ds.length === 0) {
    body.append(el("p", { class: "empty" }, ["No drives configured."]));
  } else {
    body.append(
      el(
        "div",
        { class: "lib-grid drives" },
        ds.map((d) => {
          // The cell shows just the loaded cartridge; its home slot (the
          // storage slot it returns to on unload) goes in the tooltip
          // rather than cluttering the cell.
          const title =
            d.barcode && d.home_slot != null
              ? `Drive ${d.id} · ${d.barcode} · home slot ${d.home_slot}`
              : undefined;
          return slotCell(`Drive ${d.id}`, d.barcode, { wide: true, title });
        }),
      ),
    );
  }

  // Storage slots — full grid when small enough, occupied-only beyond
  // the cap (with a note so nothing is silently dropped).
  body.append(libSection(`Storage — ${filled} ${filled === 1 ? "cartridge" : "cartridges"}`));
  if (storageTotal === 0) {
    body.append(el("p", { class: "empty" }, ["No storage slots."]));
  } else if (storageTotal <= SLOT_CELL_CAP) {
    const cells = [];
    for (let i = 0; i < storageTotal; i++) {
      const bc = storageBySlot[i];
      const home = !bc && homeOfSlot[i];
      const opts = home
        ? { home: true, title: `Slot ${i} · home of ${home.barcode} (Drive ${home.id})` }
        : undefined;
      cells.push(slotCell(i, bc, opts));
    }
    body.append(el("div", { class: "lib-grid" }, cells));
  } else {
    const occ = Object.keys(storageBySlot)
      .map(Number)
      .sort((a, b) => a - b);
    const shown = occ.slice(0, SLOT_CELL_CAP);
    body.append(
      el(
        "div",
        { class: "lib-grid" },
        shown.map((i) => slotCell(i, storageBySlot[i])),
      ),
    );
    let note = `${storageTotal} slots total — showing the ${shown.length} occupied (empty slots hidden for large libraries)`;
    if (occ.length > shown.length) note += `; +${occ.length - shown.length} more occupied not shown`;
    body.append(el("p", { class: "panel-note" }, [note]));
  }

  // Import/Export (mail) slots, if the chassis has any.
  if (mailTotal > 0) {
    body.append(libSection(`Import/Export (${mailQueue.length} / ${mailTotal})`));
    const cells = [];
    for (let i = 0; i < mailTotal; i++) cells.push(slotCell(`I/E ${i}`, mailQueue[i], { wide: true }));
    body.append(el("div", { class: "lib-grid drives" }, cells));
  }
}

// ---- inventory: VSA ------------------------------------------------------

function renderVsa(data) {
  const vols = (data.volumes && data.volumes.volumes) || [];
  $("inventory-title").textContent = "Volumes";
  $("inventory-count").textContent = `${vols.length} total`;
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

function renderStorage(mon) {
  // The backend list comes from the pool's per-backend "global" rows
  // (namespace === null) — always present, even at zero bytes. The op
  // counters come from the storage section, which only lists a backend
  // once it has actually done I/O; a missing entry means zero.
  const globals = (mon.pool || []).filter((r) => r.namespace == null);
  const ops = {};
  for (const s of mon.storage || []) ops[s.backend] = s;
  $("storage-count").textContent = globals.length ? `${globals.length}` : "";
  const body = $("storage-body");
  clear(body);
  if (globals.length === 0) {
    body.append(el("p", { class: "empty" }, ["No storage backends configured."]));
    return;
  }
  const rows = globals.map((g) => {
    const s = ops[g.backend] || {};
    const cap = g.cap_bytes || 0;
    // Cache unit lives in the column header, so the values stay short
    // ("0 / 7.58") and the 5-column table fits the side panel.
    const cache = cap > 0 ? `${gib(g.used_bytes)} / ${gib(cap)}` : gib(g.used_bytes);
    const errs = s.errors_total || 0;
    return el("tr", null, [
      el("td", { class: "mono" }, [g.backend]),
      el("td", { class: "num dim" }, [cache]),
      el("td", { class: "num" }, [num(s.put_ops_total || 0)]),
      el("td", { class: "num" }, [num(s.get_ops_total || 0)]),
      el("td", { class: errs > 0 ? "num has-err" : "num dim" }, [num(errs)]),
    ]);
  });
  body.append(
    table(
      [
        { label: "Backend" },
        { label: "Cache (GiB)", cls: "num" },
        { label: "PUTs", cls: "num" },
        { label: "GETs", cls: "num" },
        { label: "Err", cls: "num" },
      ],
      rows,
      "sb-table",
    ),
  );
}

function renderAudit(data) {
  const entries = (data && data.entries) || [];
  $("audit-count").textContent = entries.length ? `last ${AUDIT_LINES}` : "";
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
      const [info, cartridges, drives] = await Promise.all([
        api("/api/v1/library/info").catch(() => ({})),
        api("/api/v1/cartridges").catch(() => ({})),
        api("/api/v1/drives").catch(() => ({})),
      ]);
      renderLibrary({ info, cartridges, drives });
    } else {
      const volumes = await api("/api/v1/volumes").catch(() => ({}));
      renderVsa({ volumes });
    }

    renderStorage(mon);
    const audit = await api(`/api/v1/audit/tail?lines=${AUDIT_LINES}`).catch(() => ({ entries: [] }));
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
    const kind =
      PRODUCT === "thurvtl" ? "Virtual Tape Library" : "Virtual Storage Appliance";
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
