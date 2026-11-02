// Sleuth window logic. Talks to sleuth-worker.exe through the Rust shell:
//   invoke("call", {req})           one request, one reply
//   invoke("start_search", {...})   streaming search; events arrive as "worker-event"
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);
const call = (req) => invoke("call", { req });

const ROW_H = 30;
const CLASS_LABEL = { exact: "Exact", separators_ignored: "Separators ignored", uncertain: "Verify, OCR unsure" };
const CLASS_PILL = { exact: "exact", separators_ignored: "sep", uncertain: "verify" };
const CLASS_ORDER = { exact: 0, separators_ignored: 1, uncertain: 2 };
const SOURCE_LABEL = { file_name: "File name", text_layer: "PDF text", ocr: "OCR", pending: "Waiting to be read" };

const state = {
  settings: null,
  drives: [],
  driveOn: new Map(),       // root -> checked
  driveState: new Map(),    // root -> {cls, text}
  hits: new Map(),          // lower path -> hit
  view: [],                 // filtered + sorted
  sortKey: "class",
  asc: true,
  sel: -1,
  searchId: 0,
  deepRunning: false,
  queued: 0,
  query: "",
  lastDone: null,
  pv: null,                 // {hit, page, pages, boxes, cur, zoom}
  updateChannel: "stable",
};

// ---------- helpers ----------
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}
function fmtDate(secs) {
  if (!secs) return "";
  const d = new Date(secs * 1000);
  return d.toLocaleDateString(undefined, { day: "2-digit", month: "short", year: "numeric" });
}
function fmtSize(n) {
  if (n < 1024) return n + " B";
  if (n < 1048576) return (n / 1024).toFixed(0) + " KB";
  return (n / 1048576).toFixed(1) + " MB";
}
function folderOf(p) { const i = p.lastIndexOf("\\"); return i > 0 ? p.slice(0, i) : p; }
function extOf(name) { const i = name.lastIndexOf("."); return i > 0 ? name.slice(i + 1).toUpperCase().slice(0, 4) : ""; }
function isPdf(name) { return /\.pdf$/i.test(name); }
function toast(msg) { $("sbSearch").textContent = msg; }
function debounce(fn, ms) { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; }
async function copyText(t) {
  try { await navigator.clipboard.writeText(t); toast("Copied"); }
  catch { toast("Could not copy"); }
}

// ---------- worker connection ----------
async function waitForWorker() {
  for (let i = 0; i < 60; i++) {
    try { await call({ req: "hello", protocol: 1 }); return true; }
    catch { await new Promise((r) => setTimeout(r, 500)); }
  }
  showBanner("The Sleuth reader did not start. Close Sleuth and open it again.", null);
  return false;
}

// ---------- drives / scopes ----------
async function loadDrives() {
  const r = await call({ req: "drives" });
  state.drives = r.drives.filter((d) => d.kind === "fixed" || d.kind === "network");
  for (const d of state.drives) if (!state.driveOn.has(rootOf(d))) state.driveOn.set(rootOf(d), true);
  renderDrives();
}
function rootOf(d) { return d.letter + "\\"; }
function renderDrives() {
  for (const [boxId, kind] of [["localDrives", "fixed"], ["netDrives", "network"]]) {
    const box = $(boxId);
    box.replaceChildren();
    const list = state.drives.filter((d) => d.kind === kind);
    if (!list.length) box.append(el("div", "dim", "None"));
    for (const d of list) {
      const root = rootOf(d);
      const lab = el("label");
      const cb = el("input"); cb.type = "checkbox"; cb.checked = state.driveOn.get(root) !== false;
      cb.onchange = () => { state.driveOn.set(root, cb.checked); runSearch(false); };
      lab.append(cb, el("span", "drv-name", d.letter));
      if (d.unc) { const u = el("span", "drv-unc", d.unc.replace(/^\\\\/, "")); u.title = d.unc; lab.append(u); }
      else lab.append(el("span", "drv-unc", ""));
      const st = state.driveState.get(d.unc ? d.unc.toLowerCase() + "\\" : root.toLowerCase());
      if (st) lab.append(el("span", "drv-state " + st.cls, st.text));
      box.append(lab);
    }
  }
}
function selectedRoots() {
  const roots = state.drives.map(rootOf);
  const on = roots.filter((r) => state.driveOn.get(r) !== false);
  return on.length === roots.length ? [] : on;
}
function renderScopes() {
  const box = $("scopes");
  box.replaceChildren();
  const scopes = state.settings?.content_scopes || [];
  if (!scopes.length) box.append(el("div", "none", "None yet. Add the project folders whose PDFs should be read."));
  for (const s of scopes) {
    const row = el("div", "scope");
    const t = el("span", null, s); t.title = s;
    const x = el("button", "icon", "×"); x.title = "Stop reading this folder"; x.setAttribute("aria-label", "Remove " + s);
    x.onclick = async () => { await saveSettings({ ...state.settings, content_scopes: scopes.filter((v) => v !== s) }); };
    row.append(t, x); box.append(row);
  }
}
$("addScope").onclick = async () => {
  const p = await invoke("pick_folder");
  if (!p) return;
  const scopes = state.settings.content_scopes || [];
  if (!scopes.some((s) => s.toLowerCase() === p.toLowerCase())) {
    await saveSettings({ ...state.settings, content_scopes: [...scopes, p] });
    await call({ req: "rescan_now" });
    toast("Added. Its PDFs will be read in the background.");
  }
};

// ---------- searching ----------
async function runSearch(deep) {
  const q = $("q").value.trim();
  if (state.searchId) { call({ req: "cancel", search_id: state.searchId }).catch(() => {}); }
  state.query = q;
  state.hits.clear(); state.sel = -1; state.queued = 0; state.lastDone = null;
  setDeepRunning(false);
  $("deepbar").hidden = true;
  state.driveState.clear(); renderDrives();
  if (!q) { state.searchId = 0; refreshView(); clearPreview(); return; }
  try {
    state.searchId = await invoke("start_search", { query: q, roots: selectedRoots(), deep });
    if (deep) { setDeepRunning(true); $("deepbar").hidden = false; $("deepbar").replaceChildren(); }
    toast(deep ? "Deep search running…" : "Searching…");
  } catch (e) { toast(String(e)); }
  refreshView();
}
const searchSoon = debounce(() => runSearch(false), 250);
$("q").addEventListener("input", searchSoon);
$("q").addEventListener("keydown", (e) => {
  if (e.key === "Enter") { e.preventDefault(); runSearch(e.ctrlKey); }
  if (e.key === "Escape") { $("q").value = ""; runSearch(false); }
  if (e.key === "ArrowDown") { e.preventDefault(); $("rows").focus(); move(1); }
});
$("clearQ").onclick = () => { $("q").value = ""; runSearch(false); $("q").focus(); };
$("mode").onchange = () => refreshView();
for (const id of ["f-exact", "f-sep", "f-unc", "f-content", "f-names"]) $(id).onchange = () => refreshView();
$("deep").onclick = () => {
  if (state.deepRunning) { call({ req: "cancel", search_id: state.searchId }).catch(() => {}); setDeepRunning(false); toast("Deep search stopped"); return; }
  if (!$("q").value.trim()) { $("q").focus(); toast("Type what to look for first"); return; }
  runSearch(true);
};
function setDeepRunning(on) {
  state.deepRunning = on;
  $("deep").textContent = on ? "Stop" : "Deep search";
  $("deep").classList.toggle("running", on);
}

function addHits(list) {
  for (const h of list) {
    const k = h.path.toLowerCase();
    const old = state.hits.get(k);
    if (!old || CLASS_ORDER[h.class] < CLASS_ORDER[old.class] || (old.source === "file_name" && h.source !== "file_name")) state.hits.set(k, h);
  }
  refreshView();
}

listen("worker-event", (e) => {
  const v = e.payload;
  if (v.id !== state.searchId) return;
  switch (v.ev) {
    case "hits": addHits(v.hits); break;
    case "resolved":
      state.queued = Math.max(0, state.queued - 1);
      if (v.hits.length) addHits(v.hits);
      updateSearchStatus();
      break;
    case "progress": renderProgress(v.roots); break;
    case "done":
      state.lastDone = v; setDeepRunning(false); updateSearchStatus(); break;
    case "error": toast(v.message); setDeepRunning(false); break;
  }
});
listen("worker-down", () => { $("sbReader").textContent = "Reader restarting…"; });
listen("worker-up", () => { loadStatus(); });

function renderProgress(roots) {
  const bar = $("deepbar");
  bar.hidden = false;
  bar.replaceChildren();
  let queuedTotal = 0;
  for (const p of roots) {
    const cls = { done: "done", offline: "off", stopped: "stop" }[p.state] || "";
    const box = el("div", "dp " + cls);
    const lbl = el("span", "lbl", p.label); lbl.title = p.root;
    const txt = p.state === "offline" ? "Offline, skipped" : p.state === "done" ? `Done · ${p.hits} found` : p.state === "stopped" ? "Stopped" : `${p.folders_done.toLocaleString()} folders · ${p.hits} found`;
    box.append(lbl, el("span", "dim", txt));
    const trk = el("div", "trk"); trk.append(el("div", "fill")); box.append(trk);
    if (p.skipped.length && p.state !== "offline") {
      const s = el("span", "skips", `${p.skipped.length} folder${p.skipped.length > 1 ? "s" : ""} not searched`);
      s.title = p.skipped.slice(0, 30).map(([d, r]) => `${d}: ${r}`).join("\n");
      box.append(s);
    }
    bar.append(box);
    const key = p.root.toLowerCase();
    state.driveState.set(key.endsWith("\\") ? key : key + "\\", { cls: cls === "done" ? "done" : cls === "off" ? "off" : cls === "stop" ? "" : "run", text: p.state === "offline" ? "offline" : p.state === "done" ? "done" : p.state === "stopped" ? "stopped" : "searching" });
    queuedTotal += 0;
  }
  renderDrives();
  updateSearchStatus();
}

function updateSearchStatus() {
  const n = state.view.length;
  $("sbCount").textContent = state.query ? `${n.toLocaleString()} result${n === 1 ? "" : "s"}` : "Ready";
  if (state.deepRunning) toast("Deep search running…");
  else if (state.lastDone) toast(`Finished in ${(state.lastDone.millis / 1000).toFixed(1)} s` + (state.queued ? ` · ${state.queued} PDFs still being read` : ""));
}

// ---------- results view ----------
function keep(h) {
  const mode = $("mode").value;
  if (mode === "exact" && h.class !== "exact") return false;
  if (h.class === "exact" && !$("f-exact").checked) return false;
  if (h.class === "separators_ignored" && !$("f-sep").checked) return false;
  if (h.class === "uncertain" && !$("f-unc").checked) return false;
  if (h.source === "file_name" && !$("f-names").checked) return false;
  if (h.source !== "file_name" && !$("f-content").checked) return false;
  return true;
}
function sortValue(h, k) {
  switch (k) {
    case "name": return h.name.toLowerCase();
    case "folder": return folderOf(h.display_path).toLowerCase();
    case "found": return (h.found || "").toLowerCase();
    case "page": return h.page || 0;
    case "class": return CLASS_ORDER[h.class] * 10 + (h.source === "file_name" ? 1 : 0);
    case "modified": return h.modified;
  }
  return 0;
}
function refreshView() {
  const selPath = state.view[state.sel]?.path;
  const k = state.sortKey, dir = state.asc ? 1 : -1;
  state.view = [...state.hits.values()].filter(keep).sort((a, b) => {
    const va = sortValue(a, k), vb = sortValue(b, k);
    if (va < vb) return -dir; if (va > vb) return dir;
    return b.modified - a.modified;
  });
  state.sel = selPath ? state.view.findIndex((h) => h.path === selPath) : -1;
  $("spacer").style.height = state.view.length * ROW_H + "px";
  for (const b of document.querySelectorAll(".thead button")) {
    b.classList.toggle("sorted", b.dataset.k === k);
    b.classList.toggle("desc", b.dataset.k === k && !state.asc);
  }
  renderRows();
  renderEmpty();
  updateSearchStatus();
}
function renderRows() {
  const box = $("rows"), spacer = $("spacer");
  const top = box.scrollTop, h = box.clientHeight;
  const first = Math.max(0, Math.floor(top / ROW_H) - 5);
  const last = Math.min(state.view.length, Math.ceil((top + h) / ROW_H) + 5);
  spacer.replaceChildren();
  for (let i = first; i < last; i++) spacer.append(rowEl(state.view[i], i));
}
function rowEl(hit, i) {
  const r = el("div", "rrow" + (i === state.sel ? " on" : ""));
  r.style.top = i * ROW_H + "px";
  r.setAttribute("role", "row");
  const nm = el("div", "nm");
  const ext = extOf(hit.name);
  nm.append(el("span", "ext" + (ext === "PDF" ? " pdf" : ""), ext || "FILE"), el("span", null, hit.name));
  nm.title = hit.name;
  const folder = el("div", "dim", folderOf(hit.display_path)); folder.title = hit.display_path;
  const found = el("div", "mono", hit.source === "file_name" ? "" : hit.found); found.title = hit.found;
  const pg = el("div", "num", hit.page ? String(hit.page) : "");
  const m = el("div");
  m.append(el("span", "pill " + (hit.source === "file_name" ? "name" : CLASS_PILL[hit.class]), hit.source === "file_name" ? "Name · " + CLASS_LABEL[hit.class] : CLASS_LABEL[hit.class]));
  m.title = SOURCE_LABEL[hit.source] + (hit.source === "ocr" ? ` · confidence ${Math.round(hit.confidence * 100)}%` : "") + (hit.hits > 1 ? ` · ${hit.hits} matching lines` : "");
  const mod = el("div", "dim", fmtDate(hit.modified));
  r.append(nm, folder, found, pg, m, mod);
  r.onclick = () => select(i);
  r.ondblclick = () => openHit(hit);
  r.oncontextmenu = (ev) => { ev.preventDefault(); select(i); showMenu(ev.clientX, ev.clientY); };
  return r;
}
function renderEmpty() {
  const box = $("empty");
  box.replaceChildren();
  if (state.view.length) { box.hidden = true; return; }
  box.hidden = false;
  if (!state.query) {
    box.append(el("h2", null, "Find any tag, in any drawing, on any drive"));
    box.append(el("p", null, "Type a tag number, document number or any text printed on a drawing."));
    const p = el("p", "tags", '10"-P-1001-A1B   ·   PSV-1203   ·   DOC-1001-15-1659');
    box.append(p);
    if (!(state.settings?.content_scopes || []).length) box.append(el("p", null, "To search inside PDFs, add your project folders under “Folders being read” on the left."));
    return;
  }
  if (state.deepRunning) { box.append(el("p", null, "Searching the drives…")); return; }
  box.append(el("h2", null, "Nothing found yet"));
  box.append(el("p", null, state.lastDone && state.lastDone.total === 0 && state.hits.size === 0
    ? "Not in the files read so far. Deep search walks the drives and reads any PDFs it finds that have not been read."
    : "Some results are hidden by the filters on the left."));
}
$("rows").addEventListener("scroll", () => renderRows());
window.addEventListener("resize", () => renderRows());
for (const b of document.querySelectorAll(".thead button")) {
  b.onclick = () => { state.asc = state.sortKey === b.dataset.k ? !state.asc : true; state.sortKey = b.dataset.k; refreshView(); };
}
function select(i) {
  if (i < 0 || i >= state.view.length) return;
  state.sel = i;
  const box = $("rows");
  const y = i * ROW_H;
  if (y < box.scrollTop) box.scrollTop = y;
  else if (y + ROW_H > box.scrollTop + box.clientHeight) box.scrollTop = y + ROW_H - box.clientHeight;
  renderRows();
  showPreview(state.view[i]);
}
function move(d) { select(Math.min(state.view.length - 1, Math.max(0, state.sel + d))); }
$("rows").addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown") { e.preventDefault(); move(1); }
  else if (e.key === "ArrowUp") { e.preventDefault(); if (state.sel <= 0) $("q").focus(); else move(-1); }
  else if (e.key === "PageDown") { e.preventDefault(); move(10); }
  else if (e.key === "PageUp") { e.preventDefault(); move(-10); }
  else if (e.key === "Enter" && state.view[state.sel]) openHit(state.view[state.sel]);
});

// ---------- actions ----------
async function openHit(h) { try { await invoke("open_path", { path: h.display_path }); } catch (e) { toast(String(e)); } }
async function revealHit(h) { try { await invoke("reveal_path", { path: h.display_path }); } catch (e) { toast(String(e)); } }
function showMenu(x, y) {
  const m = $("ctx");
  const h = state.view[state.sel];
  m.querySelector('[data-act="read"]').hidden = !(h && isPdf(h.name));
  m.hidden = false;
  m.style.left = Math.min(x, innerWidth - 200) + "px";
  m.style.top = Math.min(y, innerHeight - 220) + "px";
}
document.addEventListener("click", () => { $("ctx").hidden = true; });
$("ctx").addEventListener("click", async (e) => {
  const act = e.target.dataset.act, h = state.view[state.sel];
  if (!act || !h) return;
  if (act === "open") openHit(h);
  if (act === "reveal") revealHit(h);
  if (act === "copy") copyText(h.display_path);
  if (act === "copyunc") copyText(h.path);
  if (act === "copytext") copyText(h.found);
  if (act === "read") {
    try { await call({ req: "read_now", path: h.path }); toast("Queued for reading first"); } catch (err) { toast(String(err)); }
  }
});
document.addEventListener("keydown", (e) => {
  if (e.ctrlKey && e.key.toLowerCase() === "f") { e.preventDefault(); $("q").focus(); $("q").select(); }
});

// ---------- preview ----------
function clearPreview() { state.pv = null; $("pvBody").hidden = true; $("pvEmpty").hidden = false; }
async function showPreview(hit, page) {
  $("pvEmpty").hidden = true; $("pvBody").hidden = false;
  const meta = $("pvMeta"); meta.replaceChildren();
  const add = (k, v) => { meta.append(el("dt", null, k), el("dd", null, v)); };
  add("File", hit.name);
  add("Folder", folderOf(hit.display_path));
  if (hit.path !== hit.display_path) add("Network path", folderOf(hit.path));
  add("Found", hit.source === "file_name" ? "In the file name" : `${hit.found}`);
  add("Read from", SOURCE_LABEL[hit.source] + (hit.source === "ocr" ? ` · confidence ${Math.round(hit.confidence * 100)}%` : ""));
  add("Size · modified", `${fmtSize(hit.size)} · ${fmtDate(hit.modified)}`);
  $("pvOpen").onclick = () => openHit(hit);
  $("pvReveal").onclick = () => revealHit(hit);
  $("pvCopy").onclick = () => copyText(hit.display_path);
  const stage = $("pvStage"), canvas = $("pvCanvas");
  canvas.querySelectorAll(".mark").forEach((m) => m.remove());
  if (!isPdf(hit.name)) {
    $("pvImg").removeAttribute("src");
    $("pvHitInfo").textContent = ""; $("pvPageInfo").textContent = "Preview is shown for PDFs";
    return;
  }
  const pg = page || hit.page || 1;
  let loading = stage.querySelector(".pv-loading");
  if (!loading) { loading = el("div", "pv-loading", "Loading page…"); stage.append(loading); }
  state.pv = { hit, page: pg, pages: 0, boxes: [], cur: 0, zoom: false };
  try {
    const r = await call({ req: "preview", path: hit.path, page: pg, query: state.query, max_px: 3000 });
    if (state.pv?.hit !== hit) return;
    state.pv.pages = r.pages; state.pv.boxes = r.boxes;
    $("pvImg").src = "data:image/png;base64," + r.png_base64;
    for (const [i, b] of r.boxes.entries()) {
      const m = el("div", "mark" + (i === 0 ? " cur" : ""));
      // Pad the box a little so a thin line of text is clearly circled.
      Object.assign(m.style, { left: `calc(${b.x * 100}% - 4px)`, top: `calc(${b.y * 100}% - 4px)`, width: `calc(${b.w * 100}% + 8px)`, height: `calc(${b.h * 100}% + 8px)` });
      canvas.append(m);
    }
    $("pvPageInfo").textContent = `Page ${pg} of ${r.pages}`;
    $("pvHitInfo").textContent = r.boxes.length ? `Mark 1 of ${r.boxes.length}` : (r.source === "pending" ? "Not read yet" : "No mark on this page");
    applyZoom(r.boxes.length > 0);
  } catch (e) {
    $("pvImg").removeAttribute("src");
    $("pvPageInfo").textContent = String(e);
  } finally { loading.remove(); }
}
function applyZoom(zoom) {
  const pv = state.pv; if (!pv) return;
  pv.zoom = zoom;
  const canvas = $("pvCanvas"), stage = $("pvStage");
  const b = pv.boxes[pv.cur];
  if (zoom && b) {
    // Show the mark about a third of the stage wide, centred.
    const scale = Math.min(8, Math.max(1.5, 0.33 / Math.max(b.w, 0.01)));
    canvas.style.width = scale * 100 + "%";
    requestAnimationFrame(() => {
      const W = canvas.clientWidth, H = canvas.clientHeight;
      stage.scrollLeft = (b.x + b.w / 2) * W - stage.clientWidth / 2;
      stage.scrollTop = (b.y + b.h / 2) * H - stage.clientHeight / 2;
    });
  } else {
    canvas.style.width = "100%";
    stage.scrollLeft = 0; stage.scrollTop = 0;
  }
  canvas.querySelectorAll(".mark").forEach((m, i) => m.classList.toggle("cur", i === pv.cur));
  $("pvZoom").textContent = zoom ? "Fit page" : "Zoom to mark";
}
$("pvZoom").onclick = () => applyZoom(!state.pv?.zoom);
function stepHit(d) {
  const pv = state.pv; if (!pv || !pv.boxes.length) return;
  pv.cur = (pv.cur + d + pv.boxes.length) % pv.boxes.length;
  $("pvHitInfo").textContent = `Mark ${pv.cur + 1} of ${pv.boxes.length}`;
  applyZoom(true);
}
$("pvPrevHit").onclick = () => stepHit(-1);
$("pvNextHit").onclick = () => stepHit(1);
$("pvPrevPage").onclick = () => { const pv = state.pv; if (pv && pv.page > 1) showPreview(pv.hit, pv.page - 1); };
$("pvNextPage").onclick = () => { const pv = state.pv; if (pv && pv.page < pv.pages) showPreview(pv.hit, pv.page + 1); };

// ---------- status ----------
async function loadStatus() {
  try {
    const r = await call({ req: "status" });
    const s = r.status;
    let t = s.reader_state === "reading" && s.current_file ? `Reading ${s.current_file.split("\\").pop()}` : cap(s.reader_state);
    t += ` · ${s.pdfs_read.toLocaleString()} PDFs read · ${s.pdfs_waiting.toLocaleString()} waiting`;
    if (s.pages_per_hour) t += ` · ${s.pages_per_hour} pages/h`;
    $("sbReader").textContent = t;
    state.lastStatus = r;
    enforceCache(s.cache);
  } catch { /* worker restarting */ }
}

// ---------- shared reading cache: required on every PC ----------
// Not set up -> a message that cannot be dismissed. Ignored for a minute ->
// Settings opens with the cache section highlighted. Closing Settings without
// fixing it starts the same cycle again. A share that is only temporarily
// unreachable gets a quiet notice instead.
const CACHE_NAG_MS = 60 * 1000;
let cacheNagSince = 0;
function cacheNeedsUser(c) { return c && (c.state === "not_set" || c.state === "no_access"); }
function enforceCache(c) {
  if (!c) return;
  const b = $("cacheBanner");
  if (cacheNeedsUser(c)) {
    $("cacheBannerText").textContent = `The shared reading cache isn't working on this PC: ${c.message}. Set it up so drawings already read on other PCs aren't read again here.`;
    $("cacheBannerAction").hidden = false; $("cacheBannerClose").hidden = true;
    b.hidden = false;
    if (!cacheNagSince) cacheNagSince = Date.now();
    if (Date.now() - cacheNagSince > CACHE_NAG_MS && !$("settingsDlg").open) openSettings(true);
  } else if (c.state === "offline") {
    cacheNagSince = 0;
    if (!b.dataset.dismissed) {
      $("cacheBannerText").textContent = `The shared reading cache (${c.path}) can't be reached right now. Sleuth keeps reading on this PC and will use it again when it's back.`;
      $("cacheBannerAction").hidden = true; $("cacheBannerClose").hidden = false;
      b.hidden = false;
    }
  } else {
    cacheNagSince = 0;
    b.hidden = true;
  }
}
$("cacheBannerAction").onclick = () => openSettings(true);
$("cacheBannerClose").onclick = () => { $("cacheBanner").hidden = true; $("cacheBanner").dataset.dismissed = "1"; };
function highlightCache() {
  const sec = $("cacheSection");
  sec.classList.remove("attention"); void sec.offsetWidth; sec.classList.add("attention");
  sec.scrollIntoView({ block: "center" });
  $("s-cache").focus(); $("s-cache").select();
}
function showCacheCheck(c) {
  const p = $("s-cacheState");
  p.className = c.state === "ok" ? "hint ok-text" : "hint error";
  p.textContent = c.state === "ok" ? `Working: ${c.path}` : c.state === "offline" ? `Can't be reached right now: ${c.message}` : `Not usable: ${c.message}`;
}
async function testCache() {
  const dir = $("s-cache").value.trim();
  $("s-cacheState").className = "hint"; $("s-cacheState").textContent = "Testing…";
  const r = await call({ req: "check_cache", dir });
  showCacheCheck(r.check);
  return r.check;
}
$("s-cacheTest").onclick = () => testCache().catch((e) => { $("s-cacheState").textContent = String(e); });
$("settingsDlg").addEventListener("close", () => { if (cacheNeedsUser(state.lastStatus?.status?.cache)) cacheNagSince = Date.now(); });
function cap(s) { return s ? s[0].toUpperCase() + s.slice(1) : ""; }
$("sbReader").onclick = async () => {
  await loadStatus();
  const r = state.lastStatus; if (!r) return;
  const s = r.status;
  const grid = $("stGrid"); grid.replaceChildren();
  for (const [l, v] of [["Files indexed", s.files_indexed], ["PDFs read", s.pdfs_read], ["Waiting to be read", s.pdfs_waiting], ["Could not be read", s.pdfs_failed]]) {
    const c = el("div", "stat"); c.append(el("div", "l", l), el("div", "v", Number(v).toLocaleString())); grid.append(c);
  }
  const f = $("stFailed"); f.replaceChildren();
  if (!r.failed.length) f.append(el("div", "dim", "None"));
  for (const x of r.failed) { const row = el("div"); const p = el("span", "p", x.path); p.title = x.path; row.append(p, el("span", "r", x.reason)); f.append(row); }
  $("statusDlg").showModal();
};
$("stClose").onclick = () => $("statusDlg").close();

// ---------- settings ----------
async function loadSettings() {
  const r = await call({ req: "get_settings" });
  state.settings = r.settings;
  state.updateChannel = r.settings.update_channel || "stable";
  renderScopes();
}
async function saveSettings(s) {
  await call({ req: "set_settings", settings: s });
  state.settings = s;
  state.updateChannel = s.update_channel;
  renderScopes();
  try { await invoke("set_autostart", { enabled: !!s.keep_indexing_when_closed }); } catch { /* registry blocked */ }
}
$("settingsBtn").onclick = () => openSettings(false);
async function openSettings(focusCache) {
  await loadSettings();
  const s = state.settings;
  $("s-quality").value = s.ocr_quality === "high" ? "high" : "auto"; $("s-dpi").value = String(s.ocr_dpi);
  $("s-busy").value = s.busy_cpu_percent; $("s-keep").checked = s.keep_indexing_when_closed;
  $("s-cache").value = s.shared_cache_dir || ""; $("s-per").value = s.listings_per_server;
  $("s-timeout").value = s.listing_timeout_secs; $("s-rescan").value = s.name_rescan_minutes;
  $("s-excluded").value = (s.excluded || []).join("\n"); $("s-channel").value = s.update_channel;
  $("s-version").textContent = "Version " + (await invoke("app_version"));
  $("s-error").hidden = true; $("s-updateMsg").textContent = "";
  const c = state.lastStatus?.status?.cache;
  if (c) showCacheCheck(c); else $("s-cacheState").textContent = "";
  $("settingsDlg").showModal();
  if (focusCache || cacheNeedsUser(c)) highlightCache();
}
$("s-cachePick").onclick = async () => { const p = await invoke("pick_folder"); if (p) { $("s-cache").value = p; testCache().catch(() => {}); } };
$("s-save").onclick = async () => {
  const num = (id, lo, hi) => { const v = Number($(id).value); if (!Number.isFinite(v) || v < lo || v > hi) throw new Error(`Enter a number from ${lo} to ${hi}`); return Math.round(v); };
  try {
    const s = {
      ...state.settings,
      ocr_quality: $("s-quality").value,
      ocr_dpi: Number($("s-dpi").value),
      busy_cpu_percent: num("s-busy", 10, 100),
      keep_indexing_when_closed: $("s-keep").checked,
      shared_cache_dir: $("s-cache").value.trim() || null,
      listings_per_server: num("s-per", 1, 16),
      listing_timeout_secs: num("s-timeout", 5, 120),
      name_rescan_minutes: num("s-rescan", 15, 10080),
      excluded: $("s-excluded").value.split(/\r?\n/).map((x) => x.trim()).filter(Boolean),
      update_channel: $("s-channel").value,
    };
    const c = await testCache();
    if (c.state === "not_set" || c.state === "no_access") {
      highlightCache();
      throw new Error("Choose a shared folder that works before saving. Every PC needs the shared reading cache.");
    }
    await saveSettings(s);
    $("settingsDlg").close();
    loadStatus();
    toast("Settings saved");
  } catch (e) { $("s-error").textContent = e.message || String(e); $("s-error").hidden = false; }
};
$("s-rescanNow").onclick = async () => { await call({ req: "rescan_now" }); toast("Refreshing the file-name index in the background"); };
$("s-checkUpdate").onclick = async () => {
  $("s-updateMsg").textContent = "Checking…";
  try {
    const u = await invoke("check_update", { channel: $("s-channel").value });
    $("s-updateMsg").textContent = u ? `Version ${u.version} is available` : "You have the latest version";
    if (u) offerUpdate(u, $("s-channel").value);
  } catch (e) { $("s-updateMsg").textContent = String(e); }
};

// ---------- updates ----------
function showBanner(text, actionLabel, action) {
  $("bannerText").textContent = text;
  $("bannerAction").hidden = !actionLabel;
  if (actionLabel) { $("bannerAction").textContent = actionLabel; $("bannerAction").onclick = action; }
  $("banner").hidden = false;
}
$("bannerClose").onclick = () => { $("banner").hidden = true; };
function offerUpdate(u, channel) {
  showBanner(`Sleuth ${u.version} is available. Your settings and index are kept.`, "Restart and update", async () => {
    $("bannerAction").disabled = true;
    const un = await listen("update-progress", (e) => {
      const { done, total } = e.payload;
      $("bannerText").textContent = total ? `Downloading update… ${Math.round((done / total) * 100)}%` : "Downloading update…";
    });
    try { await invoke("install_update", { channel }); }
    catch (e) { $("bannerText").textContent = String(e); $("bannerAction").disabled = false; }
    finally { un(); }
  });
}
async function autoCheckUpdate() {
  try { const u = await invoke("check_update", { channel: state.updateChannel }); if (u) offerUpdate(u, state.updateChannel); }
  catch { /* offline: try again later */ }
}

// ---------- start ----------
(async function start() {
  renderEmpty();
  $("q").focus();
  if (!(await waitForWorker())) return;
  await Promise.all([loadDrives(), loadSettings(), loadStatus()]).catch((e) => toast(String(e)));
  // Apply "keep reading in the background" from the first start, not only after a save.
  if (state.settings) invoke("set_autostart", { enabled: !!state.settings.keep_indexing_when_closed }).catch(() => {});
  renderEmpty();
  setInterval(loadStatus, 4000);
  setTimeout(autoCheckUpdate, 15000);
  setInterval(autoCheckUpdate, 6 * 3600 * 1000);
})();
