// Search view: locations, the search box (projects, history, syntax help),
// the virtualised results list with paging, and the preview pane.
"use strict";

const PAGE_SIZE = 500;

// ---------- drives / scopes / projects ----------
async function loadDrives() {
  const r = await call({ req: "drives" });
  state.drives = r.drives.filter((d) => d.kind === "fixed" || d.kind === "network");
  for (const d of state.drives) if (!state.driveOn.has(rootOf(d))) state.driveOn.set(rootOf(d), true);
  renderDrives();
}
function rootOf(d) { return d.letter + "\\"; }
function renderDrives() {
  const proj = $("project").value;
  for (const [boxId, kind] of [["localDrives", "fixed"], ["netDrives", "network"]]) {
    const box = $(boxId);
    box.replaceChildren();
    box.classList.toggle("disabled", !!proj);
    const list = state.drives.filter((d) => d.kind === kind);
    if (!list.length) box.append(el("div", "dim", "None"));
    for (const d of list) {
      const root = rootOf(d);
      const lab = el("label");
      const cb = el("input"); cb.type = "checkbox"; cb.checked = state.driveOn.get(root) !== false; cb.disabled = !!proj;
      cb.onchange = () => { state.driveOn.set(root, cb.checked); runSearch(false); };
      lab.append(cb, el("span", "drv-name", d.letter));
      if (d.unc) { const u = el("span", "drv-unc", d.unc.replace(/^\\\\/, "")); u.title = d.unc; lab.append(u); }
      else lab.append(el("span", "drv-unc", ""));
      const st = state.driveState.get(d.unc ? d.unc.toLowerCase() + "\\" : root.toLowerCase());
      if (st) lab.append(el("span", "drv-state " + st.cls, st.text));
      box.append(lab);
    }
  }
  const note = $("projectNote");
  if (proj) {
    const p = (state.settings?.projects || []).find((x) => x.name === proj);
    note.replaceChildren(el("b", null, `Project: ${proj}`), el("div", "dim", "Only these folders are searched:"));
    const roots = p ? p.roots : [];
    if (!roots.length) note.append(el("div", "dim", p ? "No folders yet (Settings → Projects)." : "Defined on the office server."));
    for (const r of roots) { const d = el("div", "mono small-text", r); d.title = r; note.append(d); }
    note.hidden = false;
  } else note.hidden = true;
}
function selectedRoots() {
  if ($("project").value) return [];
  const roots = state.drives.map(rootOf);
  const on = roots.filter((r) => state.driveOn.get(r) !== false);
  return on.length === roots.length ? [] : on;
}
function renderProjects() {
  fillProjectSelect($("project"), "All locations");
  for (const s of document.querySelectorAll(".project-select")) fillProjectSelect(s, "All projects");
  renderDrives();
}
$("project").onchange = () => { renderDrives(); runSearch(false); };

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
async function runSearch(deep, append) {
  const q = append ? state.query : $("q").value.trim();
  if (state.searchId) { call({ req: "cancel", search_id: state.searchId }).catch(() => {}); }
  closeHistory();
  if (!append) {
    state.query = q;
    state.hits.clear(); state.sel = -1; state.queued = 0; state.lastDone = null;
    state.page = { offset: 0, more: false, deep, loadingMore: false };
    setDeepRunning(false);
    $("deepbar").hidden = true;
    $("noticeBanner").hidden = true;
    state.driveState.clear(); renderDrives();
  } else {
    state.page.offset += PAGE_SIZE;
    state.page.loadingMore = true;
    deep = false; // the live walk already ran; further pages come from the index
  }
  if (!q) { state.searchId = 0; refreshView(); clearPreview(); return; }
  try {
    state.searchId = await invoke("start_search", {
      query: q, roots: selectedRoots(), project: $("project").value || null, deep,
      limit: PAGE_SIZE, offset: state.page.offset,
    });
    if (deep) { setDeepRunning(true); $("deepbar").hidden = false; $("deepbar").replaceChildren(); }
    toast(append ? "Loading more…" : deep ? "Deep search running…" : "Searching…");
  } catch (e) { toast(String(e)); state.page.loadingMore = false; }
  refreshView();
}
function loadMore() { if (state.page.more && !state.page.loadingMore) runSearch(false, true); }
const searchSoon = debounce(() => runSearch(false), 250);
$("q").addEventListener("input", () => { searchSoon(); if (!$("histDrop").hidden) renderHistory(); });
$("q").addEventListener("keydown", (e) => {
  const histOpen = !$("histDrop").hidden;
  if (histOpen && (e.key === "ArrowDown" || e.key === "ArrowUp")) { e.preventDefault(); moveHistory(e.key === "ArrowDown" ? 1 : -1); return; }
  if (histOpen && e.key === "Enter" && hist.cur >= 0) { e.preventDefault(); pickHistory(hist.items[hist.cur]); return; }
  if (histOpen && e.key === "Escape") { e.preventDefault(); closeHistory(); return; }
  if (e.key === "Enter") { e.preventDefault(); runSearch(e.ctrlKey); }
  if (e.key === "Escape") { $("q").value = ""; runSearch(false); }
  if (e.key === "ArrowDown") {
    e.preventDefault();
    // Down opens recent searches; with results showing it moves into the list (Alt+Down always opens history).
    if (e.altKey || !state.view.length) openHistory();
    else { $("rows").focus(); move(1); }
  }
});
$("clearQ").onclick = () => { $("q").value = ""; runSearch(false); $("q").focus(); };
$("mode").onchange = () => refreshView();
for (const id of ["f-exact", "f-sep", "f-unc", "f-partial", "f-content", "f-names"]) $(id).onchange = () => refreshView();
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

function hitKey(h) { return (h.origin === "office" ? "office:" : "") + h.path.toLowerCase(); }
function addHits(list) {
  for (const h of list) {
    const k = hitKey(h);
    const old = state.hits.get(k);
    if (!old || CLASS_ORDER[h.class] < CLASS_ORDER[old.class] || (old.source === "file_name" && h.source !== "file_name")) state.hits.set(k, h);
  }
  refreshView();
}

listen("worker-event", (e) => {
  const v = e.payload;
  if (!v.id || v.id !== state.searchId) return;
  switch (v.ev) {
    case "hits": addHits(v.hits); break;
    case "resolved":
      state.queued = Math.max(0, state.queued - 1);
      if (v.hits.length) addHits(v.hits);
      updateSearchStatus();
      break;
    case "progress": renderProgress(v.roots); break;
    case "notice": showNotice(v.message); break;
    case "done":
      state.lastDone = v; state.page.more = !!v.more; state.page.loadingMore = false;
      setDeepRunning(false); refreshView(); break;
    case "error": toast(v.message); setDeepRunning(false); state.page.loadingMore = false; break;
  }
});

function showNotice(msg) {
  $("noticeText").textContent = msg;
  $("noticeBanner").hidden = false;
}
$("noticeClose").onclick = () => { $("noticeBanner").hidden = true; };

function renderProgress(roots) {
  const bar = $("deepbar");
  bar.hidden = false;
  bar.replaceChildren();
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
  }
  renderDrives();
  updateSearchStatus();
}

function updateSearchStatus() {
  if (state.view_ !== "search") return;
  const n = state.view.length;
  $("sbCount").textContent = state.query ? `${n.toLocaleString()} result${n === 1 ? "" : "s"}${state.page.more ? " (more available)" : ""}` : "Ready";
  if (state.deepRunning) toast("Deep search running…");
  else if (state.page.loadingMore) toast("Loading more…");
  else if (state.lastDone) toast(`Finished in ${(state.lastDone.millis / 1000).toFixed(1)} s` + (state.queued ? ` · ${state.queued} PDFs still being read` : ""));
}

// ---------- recent and pinned searches ----------
const hist = { all: [], items: [], cur: -1 };
async function openHistory() {
  try { const r = await call({ req: "history" }); hist.all = r.items || []; }
  catch (e) { hist.all = []; toast(String(e)); }
  renderHistory();
  $("histDrop").hidden = false;
}
function closeHistory() { $("histDrop").hidden = true; hist.cur = -1; }
function renderHistory() {
  const box = $("histDrop");
  box.replaceChildren();
  const q = $("q").value.trim();
  const filt = (h) => !q || h.query.toLowerCase().includes(q.toLowerCase());
  const pinned = hist.all.filter((h) => h.pinned && filt(h));
  const recent = hist.all.filter((h) => !h.pinned && filt(h)).sort((a, b) => b.at - a.at).slice(0, 25);
  hist.items = [...pinned, ...recent];
  hist.cur = Math.min(hist.cur, hist.items.length - 1);
  if (q && !hist.all.some((h) => h.pinned && h.query === q)) {
    const pin = el("button", "hist-pin-cur", `☆ Pin “${q}”`); pin.type = "button";
    pin.onmousedown = (e) => e.preventDefault();
    pin.onclick = () => setPinned(q, true);
    box.append(pin);
  }
  const section = (title, list, offset) => {
    if (!list.length) return;
    box.append(el("div", "hist-h", title));
    list.forEach((h, j) => {
      const i = offset + j;
      const row = el("div", "hist-row" + (i === hist.cur ? " on" : ""));
      row.setAttribute("role", "option");
      const t = el("span", "mono hist-q", h.query); t.title = h.query;
      const when = el("span", "dim small-text", fmtDate(h.at));
      const star = el("button", "icon star" + (h.pinned ? " on" : ""), h.pinned ? "★" : "☆"); star.type = "button";
      star.title = h.pinned ? "Unpin" : "Pin"; star.setAttribute("aria-label", star.title);
      star.onmousedown = (e) => e.preventDefault();
      star.onclick = (e) => { e.stopPropagation(); setPinned(h.query, !h.pinned); };
      row.append(t, when, star);
      row.onmousedown = (e) => e.preventDefault();
      row.onclick = () => pickHistory(h);
      box.append(row);
    });
  };
  section("Pinned", pinned, 0);
  section("Recent", recent, pinned.length);
  if (!hist.items.length && !box.childElementCount) box.append(el("div", "hist-h", q ? "No earlier search matches" : "No searches yet"));
}
function moveHistory(d) {
  if (!hist.items.length) return;
  hist.cur = (hist.cur + d + hist.items.length) % hist.items.length;
  renderHistory();
  $("histDrop").querySelector(".hist-row.on")?.scrollIntoView({ block: "nearest" });
}
function pickHistory(h) { $("q").value = h.query; closeHistory(); runSearch(false); $("q").focus(); }
async function setPinned(query, pinned) {
  try { await call({ req: "pin", query, pinned }); await openHistory(); toast(pinned ? "Pinned" : "Unpinned"); }
  catch (e) { toast(String(e)); }
}
$("histBtn").onclick = (e) => { e.stopPropagation(); if ($("histDrop").hidden) { openHistory(); $("q").focus(); } else closeHistory(); };
$("q").addEventListener("blur", () => setTimeout(() => { if (document.activeElement !== $("q")) closeHistory(); }, 150));

// ---------- query syntax help ----------
$("syntaxBtn").onclick = (e) => {
  e.stopPropagation();
  const p = $("syntaxPop"); p.hidden = !p.hidden;
  $("syntaxBtn").setAttribute("aria-expanded", String(!p.hidden));
};
$("syntaxPop").onclick = (e) => {
  e.stopPropagation();
  const code = e.target.closest("td:first-child code");
  if (code) { $("q").value = code.textContent; $("syntaxPop").hidden = true; runSearch(false); $("q").focus(); }
};

// ---------- results view ----------
function keep(h) {
  const mode = $("mode").value;
  if (mode === "exact" && h.class !== "exact") return false;
  if (h.class === "exact" && !$("f-exact").checked) return false;
  if (h.class === "separators_ignored" && !$("f-sep").checked) return false;
  if (h.class === "uncertain" && !$("f-unc").checked) return false;
  if (h.class === "partial" && !$("f-partial").checked) return false;
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
    case "class": return (CLASS_ORDER[h.class] ?? 9) * 10 + (h.source === "file_name" ? 1 : 0);
    case "modified": return h.modified;
  }
  return 0;
}
function refreshView() {
  const selKey = state.view[state.sel] ? hitKey(state.view[state.sel]) : null;
  const k = state.sortKey, dir = state.asc ? 1 : -1;
  state.view = [...state.hits.values()].filter(keep).sort((a, b) => {
    const va = sortValue(a, k), vb = sortValue(b, k);
    if (va < vb) return -dir; if (va > vb) return dir;
    return b.modified - a.modified;
  });
  state.sel = selKey ? state.view.findIndex((h) => hitKey(h) === selKey) : -1;
  $("spacer").style.height = (state.view.length + (state.page.more ? 1 : 0)) * ROW_H + "px";
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
  const n = state.view.length;
  const first = Math.max(0, Math.floor(top / ROW_H) - 5);
  const last = Math.min(n, Math.ceil((top + h) / ROW_H) + 5);
  const frag = document.createDocumentFragment();
  for (let i = first; i < last; i++) frag.append(rowEl(state.view[i], i));
  if (state.page.more && last >= n) frag.append(moreRow(n));
  spacer.replaceChildren(frag);
}
function moreRow(i) {
  const r = el("div", "rrow more");
  r.style.top = i * ROW_H + "px";
  const b = el("button", "small", state.page.loadingMore ? "Loading…" : `Load the next ${PAGE_SIZE} results`);
  b.disabled = state.page.loadingMore;
  b.onclick = loadMore;
  r.append(b);
  return r;
}
function matchTitle(hit) {
  let t = hit.why ? hit.why + "\n" : "";
  t += SOURCE_LABEL[hit.source] || hit.source;
  if (hit.source === "ocr") t += ` · confidence ${pct(hit.confidence)}`;
  if (hit.hits > 1) t += ` · ${hit.hits} matching lines`;
  return t;
}
function rowEl(hit, i) {
  const r = el("div", "rrow" + (i === state.sel ? " on" : ""));
  r.style.top = i * ROW_H + "px";
  r.setAttribute("role", "row");
  r.title = matchTitle(hit);
  const nm = el("div", "nm");
  const ext = extOf(hit.name);
  nm.append(el("span", "ext" + (ext === "PDF" ? " pdf" : ""), ext || "FILE"));
  if (hit.origin === "office") { const o = el("span", "origin", "Office"); o.title = "From the office server's index"; nm.append(o); }
  nm.append(el("span", null, hit.name));
  const folder = el("div", "dim", folderOf(hit.display_path)); folder.title = hit.display_path;
  const found = el("div", "mono", hit.source === "file_name" ? "" : hit.found);
  const pg = el("div", "num", hit.page ? String(hit.page) : "");
  const m = el("div");
  const verifyByConf = hit.source === "ocr" && hit.confidence < VERIFY_BELOW && hit.class !== "uncertain";
  m.append(el("span", "pill " + (hit.source === "file_name" ? "name" : CLASS_PILL[hit.class]), hit.source === "file_name" ? "Name · " + CLASS_LABEL[hit.class] : CLASS_LABEL[hit.class]));
  if (verifyByConf) m.append(" ", el("span", "pill verify", "verify"));
  const mod = el("div", "dim", fmtDate(hit.modified));
  r.append(nm, folder, found, pg, m, mod);
  r.onclick = () => select(i);
  r.ondblclick = () => openFile(hit.display_path);
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
    box.append(el("p", null, "Type a tag number, document number or any text printed on a drawing. Press ? next to the box for OR, rev:, dwg:, type: and name:."));
    const p = el("p", "tags", '10"-P-1001-A1B   ·   PSV-1203 OR PSV-1204   ·   rev:C dwg:ISO-1001');
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
  if (e.key === "ArrowDown") { e.preventDefault(); if (state.sel === state.view.length - 1 && state.page.more) loadMore(); else move(1); }
  else if (e.key === "ArrowUp") { e.preventDefault(); if (state.sel <= 0) $("q").focus(); else move(-1); }
  else if (e.key === "PageDown") { e.preventDefault(); move(10); }
  else if (e.key === "PageUp") { e.preventDefault(); move(-10); }
  else if (e.key === "Enter" && state.view[state.sel]) openFile(state.view[state.sel].display_path);
});

// ---------- actions ----------
function showMenu(x, y) {
  const m = $("ctx");
  const h = state.view[state.sel];
  m.querySelector('[data-act="read"]').hidden = !(h && isPdf(h.name) && h.origin !== "office");
  m.querySelector('[data-act="check"]').hidden = !(h && isPdf(h.name) && h.origin !== "office");
  m.hidden = false;
  m.style.left = Math.min(x, innerWidth - 300) + "px";
  m.style.top = Math.min(y, innerHeight - 260) + "px";
}
document.addEventListener("click", () => {
  $("ctx").hidden = true; $("exportMenu").hidden = true; $("syntaxPop").hidden = true;
  $("syntaxBtn").setAttribute("aria-expanded", "false");
});
$("ctx").addEventListener("click", async (e) => {
  const act = e.target.dataset.act, h = state.view[state.sel];
  if (!act || !h) return;
  if (act === "open") openFile(h.display_path);
  if (act === "reveal") revealFile(h.display_path);
  if (act === "copy") copyText(h.display_path);
  if (act === "copyunc") copyText(h.path);
  if (act === "copytext") copyText(h.found);
  if (act === "read") {
    try { await call({ req: "read_now", path: h.path }); toast("Queued for reading first"); } catch (err) { toast(String(err)); }
  }
  if (act === "check") { $("chkOnlyFile").checked = true; showView("check"); }
});

// ---------- export ----------
$("exportBtn").onclick = (e) => { e.stopPropagation(); $("exportMenu").hidden = !$("exportMenu").hidden; };
$("exportMenu").onclick = (e) => {
  const fmt = e.target.dataset.fmt; if (!fmt) return;
  $("exportMenu").hidden = true;
  exportResults(fmt);
};
function exportResults(fmt) {
  if (!state.view.length) { toast("Nothing to export"); return; }
  const headers = ["Name", "Folder", "Page", "Text found", "Match", "Why", "Source", "Confidence %", "Drawing no", "Revision", "Modified", "Origin"];
  const rows = state.view.map((h) => [
    h.name, folderOf(h.display_path), h.page ?? null, h.source === "file_name" ? "" : h.found,
    CLASS_LABEL[h.class] || h.class, h.why || "", SOURCE_LABEL[h.source] || h.source,
    Math.round((h.confidence ?? 1) * 100), h.drawing_no || "", h.revision || "",
    h.modified ? new Date(h.modified * 1000).toISOString().slice(0, 16).replace("T", " ") : "",
    h.origin === "office" ? "Office server" : "This PC",
  ]);
  const safe = (state.query || "results").replace(/[\\/:*?"<>|]+/g, "_").slice(0, 60);
  exportTable(fmt, `Sleuth ${safe}`, "Results", headers, rows);
  if (state.page.more) toast("Only the results loaded so far were exported. Use “Load the next 500” first to include more.");
}

// ---------- preview ----------
const pvViewer = new Viewer($("pvViewer"));
const pv = { hit: null, doc: null, tab: "page", detailsFor: null, relatedFor: null };
function clearPreview() { pv.hit = null; pv.doc = null; pvViewer.clear(); $("pvBody").hidden = true; $("pvEmpty").hidden = false; }
function isOffice(h) { return h?.origin === "office"; }

function showPreview(hit) {
  pv.hit = hit; pv.doc = hit; pv.detailsFor = null; pv.relatedFor = null;
  $("pvEmpty").hidden = true; $("pvBody").hidden = false;
  const meta = $("pvMeta"); meta.replaceChildren();
  const add = (k, v, c) => kvAdd(meta, k, v, c);
  add("File", hit.name);
  add("Folder", folderOf(hit.display_path));
  if (hit.path !== hit.display_path) add("Network path", folderOf(hit.path));
  add("Found", hit.source === "file_name" ? "In the file name" : `${hit.found}`, hit.source === "file_name" ? "" : "mono");
  const match = el("span"); match.append(el("span", "pill " + CLASS_PILL[hit.class], CLASS_LABEL[hit.class]));
  add("Match", match);
  if (hit.why) add("Why", hit.why);
  add("Read from", (SOURCE_LABEL[hit.source] || hit.source) + (hit.source === "ocr" ? ` · confidence ${pct(hit.confidence)}` : ""));
  if (hit.source === "ocr" && hit.confidence < VERIFY_BELOW) add("Check", el("span", "pill verify", "Verify on the drawing: low OCR confidence"));
  if (hit.drawing_no || hit.revision) add("Drawing · rev", `${hit.drawing_no || "–"} · ${hit.revision || "–"}`);
  add("Size · modified", `${fmtSize(hit.size)} · ${fmtDate(hit.modified)}`);
  add("Index", isOffice(hit) ? "Office server" : "This PC");
  $("pvOpen").onclick = () => openFile(hit.display_path);
  $("pvReveal").onclick = () => revealFile(hit.display_path);
  $("pvCopy").onclick = () => copyText(hit.display_path);
  pvViewer.load({ path: hit.path, name: hit.name, page: hit.page, query: state.query, office: isOffice(hit) });
  if (pv.tab !== "page") loadPreviewTab(pv.tab);
}

for (const b of document.querySelectorAll(".subtabs button")) {
  b.onclick = () => {
    pv.tab = b.dataset.pt;
    for (const x of document.querySelectorAll(".subtabs button")) { x.classList.toggle("on", x === b); x.setAttribute("aria-selected", x === b); }
    for (const t of ["page", "details", "related"]) $("pt-" + t).hidden = t !== pv.tab;
    loadPreviewTab(pv.tab);
  };
}
function loadPreviewTab(tab) {
  const d = pv.doc; if (!d) return;
  if (tab === "details" && pv.detailsFor !== d.path) { pv.detailsFor = d.path; loadDetails(d); }
  if (tab === "related" && pv.relatedFor !== d.path) { pv.relatedFor = d.path; loadRelated(d); }
}

const META_FIELDS = [
  ["drawing_no", "Drawing number", ["drawing_no", "drawing_number"]],
  ["title", "Title", ["title"]],
  ["revision", "Revision", ["revision", "rev"]],
  ["project", "Project", ["project"]],
  ["discipline", "Discipline", ["discipline"]],
  ["sheet", "Sheet", ["sheet"]],
  ["status", "Status", ["status"]],
];
async function loadDetails(doc) {
  const box = $("pt-details");
  box.replaceChildren(el("div", "dim", "Loading…"));
  if (!isPdf(doc.name)) { box.replaceChildren(el("div", "dim", "Details are shown for PDFs.")); return; }
  let r;
  try { r = await call({ req: "doc_info", path: doc.path, office: isOffice(doc) }); }
  catch (e) { if (pv.doc === doc) box.replaceChildren(el("div", "error", String(e))); return; }
  if (pv.doc !== doc) return;
  box.replaceChildren();
  box.append(el("h4", null, "Title block"));
  const md = r.metadata || {};
  const ev = (md.evidence || []).map(([f, t]) => ({ f: String(f).toLowerCase(), t, used: false }));
  const tb = el("table", "grid");
  tb.append(trow(["Field", "Value", "Evidence (as read)"], "th"));
  for (const [key, label, aliases] of META_FIELDS) {
    const evs = ev.filter((e) => aliases.includes(e.f));
    evs.forEach((e) => { e.used = true; });
    const val = md[key];
    const tr = trow([label, val ?? "", evs.map((e) => e.t).join("\n")]);
    if (!val) tr.children[1].append(el("span", "dim", "not found"));
    tr.children[1].classList.add("mono");
    tr.children[2].classList.add("evidence");
    tb.append(tr);
  }
  for (const e of ev.filter((x) => !x.used)) { const tr = trow([e.f, "", e.t]); tr.children[2].classList.add("evidence"); tb.append(tr); }
  box.append(tb);
  box.append(el("p", "hint", "Values are what was read from the drawing, with the text they came from. A blank value was not found; it is not guessed."));

  box.append(el("h4", null, `Pages (${(r.pages || []).length}) · ${fmtNum(r.tags)} tags found`));
  const pt = el("table", "grid num-cols");
  pt.append(trow(["Page", "Kind", "Chars", "Images %", "OCR tiles", "OCR ms", "Note"], "th"));
  for (const p of r.pages || []) {
    const tiles = p.ocr_tiles_skipped ? `${p.ocr_tiles} (+${p.ocr_tiles_skipped} skipped)` : String(p.ocr_tiles);
    pt.append(trow([String(p.page), PAGE_CLASS[p.class] || p.class, fmtNum(p.chars), Math.round(p.image_coverage * 100) + "%", tiles, fmtNum(p.ocr_millis), p.note || ""]));
  }
  if (!(r.pages || []).length) pt.append(trow(["", "Not read yet", "", "", "", "", ""]));
  box.append(pt);

  box.append(el("h4", null, "How this file was read"));
  const pr = r.provenance || {};
  const dl = el("dl", "kv");
  kvAdd(dl, "Obtained", pr.obtained || "");
  kvAdd(dl, "Read by", pr.read_by || "");
  kvAdd(dl, "When", pr.read_at ? fmtDateTime(pr.read_at) : "");
  kvAdd(dl, "OCR model", pr.model ? `${pr.model}${pr.model_hash ? " · " + pr.model_hash.slice(0, 12) : ""}` : "");
  kvAdd(dl, "Ran on", pr.backend || "");
  kvAdd(dl, "Resolution", pr.dpi ? pr.dpi + " dpi" : "");
  kvAdd(dl, "Reader version", pr.reader_version ? String(pr.reader_version) : "");
  const fp = el("span", "mono", pr.pipeline ? pr.pipeline.slice(0, 16) : ""); fp.title = pr.pipeline || "";
  kvAdd(dl, "Pipeline", fp);
  box.append(dl);
}
const PAGE_CLASS = { searchable: "Text layer", scanned: "Scanned", hybrid: "Text + scan", vector_outlined: "Outlined vectors", sparse: "Little text", damaged: "Damaged" };
function trow(cells, tag) {
  const tr = el("tr");
  for (const c of cells) { const td = el(tag || "td"); if (c instanceof Node) td.append(c); else td.textContent = c; tr.append(td); }
  return tr;
}

async function loadRelated(doc) {
  const box = $("pt-related");
  box.replaceChildren(el("div", "dim", "Loading…"));
  let r;
  try { r = await call({ req: "related", path: doc.path, office: isOffice(doc) }); }
  catch (e) { if (pv.doc === doc) box.replaceChildren(el("div", "error", String(e))); return; }
  if (pv.doc !== doc) return;
  box.replaceChildren();
  box.append(el("p", "hint", "Documents that share engineering tags with this one, most shared first. Click one to see it with the first shared tag marked."));
  if (!r.items.length) box.append(el("div", "dim", "No other document shares a tag with this one."));
  for (const it of r.items) {
    const card = el("button", "related");
    card.type = "button";
    const top = el("div", "rel-top");
    top.append(el("span", "rel-name", it.name), el("span", "dim small-text", `${it.shared_count} shared tag${it.shared_count === 1 ? "" : "s"}`));
    card.append(top);
    if (it.drawing_no) card.append(el("div", "dim small-text mono", it.drawing_no));
    const chips = el("div", "chips");
    for (const t of it.shared) chips.append(el("span", "chip mono", t));
    card.append(chips);
    card.title = it.display_path;
    card.onclick = () => openRelated(it, doc);
    box.append(card);
  }
}
function openRelated(it, from) {
  // Preview marks one tag (Preview.tag); the other shared tags are passed as
  // OR-alternatives in the query so they are marked too where possible.
  const simple = it.shared.filter((t) => !/[\s"]/.test(t));
  const doc = { path: it.path, display_path: it.display_path, name: it.name, page: it.page || 1, origin: from.origin, query: simple.join(" OR "), tag: normTag(it.shared[0]) };
  pv.doc = doc; pv.detailsFor = null; pv.relatedFor = null;
  const meta = $("pvMeta"); meta.replaceChildren();
  const back = el("button", "linkish", "‹ Back to " + (pv.hit?.name || "the result")); back.type = "button";
  back.onclick = () => { if (pv.hit) showPreview(pv.hit); };
  kvAdd(meta, "", back);
  kvAdd(meta, "File", it.name);
  kvAdd(meta, "Folder", folderOf(it.display_path));
  if (it.drawing_no) kvAdd(meta, "Drawing no", it.drawing_no, "mono");
  kvAdd(meta, "Shared tags", it.shared.join(", ") + (it.shared_count > it.shared.length ? ` and ${it.shared_count - it.shared.length} more` : ""), "mono");
  $("pvOpen").onclick = () => openFile(it.display_path);
  $("pvReveal").onclick = () => revealFile(it.display_path);
  $("pvCopy").onclick = () => copyText(it.display_path);
  document.querySelector('.subtabs button[data-pt="page"]').click();
  pvViewer.load({ path: it.path, name: it.name, page: it.page || 1, query: doc.query, tag: doc.tag, office: isOffice(from) });
}

viewHooks.search = { show() { updateSearchStatus(); renderRows(); } };
