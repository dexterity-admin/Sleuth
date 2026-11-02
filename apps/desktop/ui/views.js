// Tags, Revisions, Check readings and Monitor views.
"use strict";

// =====================================================================
// Tags
// =====================================================================
const TAG_PAGE = 200;
const tg = { rows: [], total: 0, gen: 0, loading: new Set(), seen: new Set(), cur: null, occ: [], loaded: false };
function tagFilter() {
  return {
    category: $("tagCat").value || null,
    contains: $("tagContains").value.trim() || null,
    project: $("tagProject").value || null,
    office: segValue("tagSource") === "office",
  };
}
const tagTable = new VTable($("tagTable"), {
  columns: [
    { label: "Tag", cls: "mono", width: "minmax(0,1.6fr)", render: (t) => t.display },
    { label: "Category", width: "minmax(0,1fr)", render: (t) => t.category },
    { label: "Documents", cls: "num", width: "84px", render: (t) => fmtNum(t.documents) },
    { label: "Places", cls: "num", width: "70px", title: "Occurrences on all pages", render: (t) => fmtNum(t.occurrences) },
    { label: "Lowest confidence", width: "130px", render: (t) => confCell(t.min_conf) },
  ],
  count: () => tg.total,
  row: (i) => tg.rows[i],
  onNeed: (i) => loadTagPage(Math.floor(i / TAG_PAGE) * TAG_PAGE),
  onSelect: (t) => loadOccurrences(t),
  onOpen: () => occTable.focus(),
  empty: () => (tg.loaded ? "No tags match these filters." : "Loading…"),
});
async function reloadTags() {
  tg.gen++; tg.rows = []; tg.total = 0; tg.loading.clear(); tg.loaded = false;
  tagTable.refresh();
  await loadTagPage(0);
}
async function loadTagPage(offset) {
  if (tg.loading.has(offset)) return;
  const gen = tg.gen;
  tg.loading.add(offset);
  try {
    const r = await call({ req: "tags", ...tagFilter(), limit: TAG_PAGE, offset });
    if (gen !== tg.gen) return;
    tg.total = Number(r.total) || 0;
    r.tags.forEach((t, j) => { tg.rows[offset + j] = t; if (!tg.seen.has(t.category)) { tg.seen.add(t.category); fillCategories(); } });
    tg.loaded = true;
    $("tagCount").textContent = `${fmtNum(tg.total)} tag${tg.total === 1 ? "" : "s"}` + (tagFilter().office ? " on the office server" : " on this PC");
    tagTable.refresh(true);
  } catch (e) {
    if (gen === tg.gen) { tg.loaded = true; $("tagCount").textContent = String(e); tagTable.refresh(true); }
  } finally { if (gen === tg.gen) tg.loading.delete(offset); }
}
function fillCategories() {
  const sel = $("tagCat"), cur = sel.value;
  const custom = (state.settings?.tag_rules || []).map((r) => r.category);
  const cats = [...new Set([...BUILTIN_CATEGORIES, ...custom, ...tg.seen])].filter(Boolean).sort();
  sel.replaceChildren(new Option("All categories", ""), ...cats.map((c) => new Option(cap(c), c)));
  sel.value = cats.includes(cur) ? cur : "";
}

const occTable = new VTable($("occTable"), {
  columns: [
    { label: "File", width: "minmax(0,1.5fr)", render: (o) => { const s = el("span", null, o.name); s.title = o.display_path; return s; } },
    { label: "Pg", cls: "num", width: "40px", render: (o) => String(o.page) },
    { label: "Read as", cls: "mono", width: "minmax(0,1fr)", render: (o) => o.raw },
    { label: "Confidence", width: "110px", render: (o) => confCell(o.conf, o.source) },
    { label: "Drawing no", cls: "mono", width: "minmax(0,1fr)", render: (o) => o.drawing_no || "" },
    { label: "Rev", width: "44px", render: (o) => o.revision || "" },
  ],
  count: () => tg.occ.length,
  row: (i) => tg.occ[i],
  onSelect: (o) => showOccurrence(o),
  onOpen: (o) => openFile(o.display_path),
  empty: () => (tg.cur ? "Loading…" : ""),
});
async function loadOccurrences(t) {
  tg.cur = t; tg.occ = [];
  $("occExportX").disabled = $("occExportC").disabled = true;
  $("occTitle").textContent = `${t.display}: loading…`;
  occTable.refresh();
  try {
    const f = tagFilter();
    const r = await call({ req: "tag_occurrences", norm: t.norm, project: f.project, office: f.office });
    if (tg.cur !== t) return;
    tg.occ = r.items;
    $("occTitle").textContent = `${t.display} · ${cap(t.category)} · found in ${fmtNum(r.items.length)} place${r.items.length === 1 ? "" : "s"}`;
    $("occExportX").disabled = $("occExportC").disabled = !r.items.length;
    occTable.refresh();
    if (r.items.length) occTable.select(0);
  } catch (e) { if (tg.cur === t) $("occTitle").textContent = String(e); }
}
const tagViewer = new Viewer($("tagViewer"), { emptyText: "Select a place to see the page with every occurrence of the tag marked." });
tagViewer.clear();
function showOccurrence(o) {
  const t = tg.cur; if (!t) return;
  const office = tagFilter().office;
  tagViewer.load({ path: o.path, name: o.name, page: o.page, tag: t.norm, query: "", office, extra: [o.bbox], extraLabel: `Read as ${o.raw}` });
  const m = $("tagMeta"); m.replaceChildren();
  kvAdd(m, "File", o.name);
  kvAdd(m, "Folder", folderOf(o.display_path));
  kvAdd(m, "Read as", o.raw, "mono");
  kvAdd(m, "Read from", `${SOURCE_LABEL[o.source] || o.source} · confidence ${pct(o.conf)}`);
  if (o.conf < VERIFY_BELOW && o.source !== "verified") kvAdd(m, "Check", el("span", "pill verify", "Verify on the drawing: low OCR confidence"));
  if (o.drawing_no || o.revision) kvAdd(m, "Drawing · rev", `${o.drawing_no || "–"} · ${o.revision || "–"}`);
  $("tagActions").hidden = false;
  $("tagOpen").onclick = () => openFile(o.display_path);
  $("tagReveal").onclick = () => revealFile(o.display_path);
}
$("tagCat").onchange = reloadTags;
$("tagProject").onchange = reloadTags;
$("tagContains").addEventListener("input", debounce(reloadTags, 250));
$("tagContains").addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown") { e.preventDefault(); tagTable.focus(); tagTable.select(Math.max(0, tagTable.sel)); }
  if (e.key === "Escape") { $("tagContains").value = ""; reloadTags(); }
});
initSeg("tagSource", reloadTags);

async function exportTags(fmt) {
  toast("Collecting tags…");
  const all = [];
  const f = tagFilter();
  try {
    for (let off = 0; ; off += 1000) {
      const r = await call({ req: "tags", ...f, limit: 1000, offset: off });
      all.push(...r.tags);
      if (!r.tags.length || all.length >= Number(r.total)) break;
    }
  } catch (e) { toast(String(e)); return; }
  const headers = ["Tag", "Normalised", "Category", "Documents", "Places", "Lowest confidence %", "Check"];
  const rows = all.map((t) => [t.display, t.norm, t.category, t.documents, t.occurrences, Math.round(t.min_conf * 100), t.min_conf < VERIFY_BELOW ? "verify" : ""]);
  exportTable(fmt, "Sleuth tags", "Tags", headers, rows);
}
function exportOcc(fmt) {
  const t = tg.cur; if (!t || !tg.occ.length) return;
  const headers = ["Tag", "File", "Folder", "Page", "Read as", "Source", "Confidence %", "Check", "Drawing no", "Revision"];
  const rows = tg.occ.map((o) => [t.display, o.name, folderOf(o.display_path), o.page, o.raw, SOURCE_LABEL[o.source] || o.source, Math.round(o.conf * 100), o.conf < VERIFY_BELOW && o.source !== "verified" ? "verify" : "", o.drawing_no || "", o.revision || ""]);
  exportTable(fmt, `Sleuth ${t.display.replace(/[\\/:*?"<>|]+/g, "_")} places`, "Places", headers, rows);
}
$("tagExportX").onclick = () => exportTags("xlsx");
$("tagExportC").onclick = () => exportTags("csv");
$("occExportX").onclick = () => exportOcc("xlsx");
$("occExportC").onclick = () => exportOcc("csv");

viewHooks.tags = {
  show() {
    syncSeg("tagSource", reloadTags);
    fillCategories();
    if (!tg.gen) reloadTags(); else tagTable.render();
    $("sbCount").textContent = "Tags";
  },
};

// =====================================================================
// Revisions
// =====================================================================
const rv = { groups: [], open: null, pick: null, cmp: null, changes: [], gen: 0 };
const revOld = new Viewer($("oldViewer"), { height: "", emptyText: "Select a change to see the old revision here." });
const revNew = new Viewer($("newViewer"), { height: "", emptyText: "Select a change to see the new revision here." });
revOld.clear(); revNew.clear();
async function loadRevisions() {
  const gen = ++rv.gen;
  $("revCount").textContent = "Loading…";
  try {
    const r = await call({ req: "revisions", contains: $("revFilter").value.trim() || null, project: $("revProject").value || null, limit: 1000, office: segValue("revSource") === "office" });
    if (gen !== rv.gen) return;
    rv.groups = r.groups;
    $("revCount").textContent = `${fmtNum(r.groups.length)} drawing number${r.groups.length === 1 ? "" : "s"}` + (r.groups.length >= 1000 ? " (first 1000; type to narrow)" : "");
    renderRevList();
  } catch (e) { if (gen === rv.gen) $("revCount").textContent = String(e); }
}
function renderRevList() {
  const box = $("revList");
  box.replaceChildren();
  if (!rv.groups.length) box.append(el("div", "dim pad", "No drawings with a drawing number found. Drawing numbers come from the title block of each PDF."));
  for (const g of rv.groups) {
    const item = el("div", "rev-item" + (rv.open === g ? " open" : ""));
    const head = el("button", "rev-head"); head.type = "button";
    head.append(el("span", "mono", g.drawing_no), el("span", "dim small-text", `${g.docs.length} rev${g.docs.length === 1 ? "" : "s"}`));
    head.onclick = () => openGroup(g);
    item.append(head);
    if (rv.open === g) {
      const tbl = el("div", "rev-docs");
      const hdr = el("div", "rev-doc hdr"); hdr.append(el("span", null, "Old"), el("span", null, "New"), el("span", null, "Rev"), el("span", null, "File"));
      tbl.append(hdr);
      g.docs.forEach((d, i) => {
        const row = el("div", "rev-doc");
        const ro = el("input"); ro.type = "radio"; ro.name = "revOld"; ro.checked = rv.pick.old === i; ro.title = "Use as the old revision"; ro.setAttribute("aria-label", "Old: " + d.name);
        const rn = el("input"); rn.type = "radio"; rn.name = "revNew"; rn.checked = rv.pick.new === i; rn.title = "Use as the new revision"; rn.setAttribute("aria-label", "New: " + d.name);
        ro.onchange = () => { rv.pick.old = i; };
        rn.onchange = () => { rv.pick.new = i; };
        const nm = el("span", "ell", d.name); nm.title = `${d.display_path}\n${d.title || ""}\nModified ${fmtDate(d.modified)}`;
        row.append(ro, rn, el("span", "mono", d.revision || "?"), nm);
        tbl.append(row);
      });
      const go = el("button", "small primary", "Compare"); go.type = "button";
      go.disabled = g.docs.length < 2;
      go.onclick = () => runCompare();
      const act = el("div", "rev-act"); act.append(go);
      if (g.docs.length < 2) act.append(el("span", "dim small-text", "Only one revision found"));
      tbl.append(act);
      item.append(tbl);
    }
    box.append(item);
  }
}
function openGroup(g) {
  if (rv.open === g) { rv.open = null; renderRevList(); return; }
  rv.open = g;
  // Default: the last two revisions (oldest first, so the last two are the newest).
  const n = g.docs.length;
  rv.pick = { old: Math.max(0, n - 2), new: n - 1 };
  renderRevList();
  if (n >= 2) runCompare();
  else { $("revTitle").textContent = `${g.drawing_no}: only one revision found.`; $("revSummary").replaceChildren(); rv.changes = []; chgTable.refresh(); }
}
async function runCompare() {
  const g = rv.open; if (!g) return;
  const o = g.docs[rv.pick.old], n = g.docs[rv.pick.new];
  if (!o || !n || o === n) { toast("Pick two different revisions"); return; }
  const office = segValue("revSource") === "office";
  rv.cmp = { g, o, n, office };
  const cmp = rv.cmp;
  $("revTitle").textContent = `${g.drawing_no}: comparing rev ${o.revision || "?"} with rev ${n.revision || "?"}…`;
  $("revSummary").replaceChildren();
  rv.changes = []; chgTable.refresh();
  revOld.clear(); revNew.clear();
  $("oldHead").textContent = `Old: rev ${o.revision || "?"} · ${o.name}`;
  $("newHead").textContent = `New: rev ${n.revision || "?"} · ${n.name}`;
  try {
    const r = await call({ req: "compare", old: o.path, new: n.path, office });
    if (rv.cmp !== cmp) return;
    cmp.result = r;
    $("revTitle").textContent = `${g.drawing_no}: rev ${r.old.revision || o.revision || "?"} → rev ${r.new.revision || n.revision || "?"}`;
    const counts = { changed: 0, added: 0, removed: 0, verify: 0 };
    for (const c of r.changes) counts[c.kind] = (counts[c.kind] || 0) + 1;
    const s = $("revSummary");
    for (const [k, label, cls] of [["changed", "Changed", "chg-changed"], ["added", "Added", "chg-added"], ["removed", "Removed", "chg-removed"], ["verify", "Verify", "verify"]]) {
      const b = el("span", "count"); b.append(el("span", "pill " + cls, label), " ", el("b", null, String(counts[k] || 0))); s.append(b);
    }
    const u = el("span", "count dim", `${fmtNum(r.unchanged)} tags unchanged`); s.append(u);
    applyChgFilter();
    if (rv.changes.length) chgTable.select(0);
  } catch (e) { if (rv.cmp === cmp) $("revTitle").textContent = String(e); }
}
function applyChgFilter() {
  const on = new Set([...document.querySelectorAll("#chgFilters input")].filter((i) => i.checked).map((i) => i.dataset.kind));
  rv.changes = (rv.cmp?.result?.changes || []).filter((c) => on.has(c.kind));
  chgTable.refresh();
}
for (const i of document.querySelectorAll("#chgFilters input")) i.onchange = applyChgFilter;
const KIND_LABEL = { changed: "Changed", added: "Added", removed: "Removed", verify: "Verify" };
const KIND_PILL = { changed: "chg-changed", added: "chg-added", removed: "chg-removed", verify: "verify" };
const chgTable = new VTable($("chgTable"), {
  columns: [
    { label: "Change", width: "86px", render: (c) => el("span", "pill " + KIND_PILL[c.kind], KIND_LABEL[c.kind] || c.kind) },
    { label: "Category", width: "minmax(0,0.8fr)", render: (c) => c.category },
    { label: "Old reading", cls: "mono", width: "minmax(0,1fr)", render: (c) => c.old ?? "" },
    { label: "New reading", cls: "mono", width: "minmax(0,1fr)", render: (c) => c.new ?? "" },
    { label: "Old conf.", width: "96px", render: (c) => confCell(c.old_conf) },
    { label: "New conf.", width: "96px", render: (c) => confCell(c.new_conf) },
    { label: "Reason", width: "minmax(0,1.6fr)", render: (c) => c.reason },
  ],
  count: () => rv.changes.length,
  row: (i) => rv.changes[i],
  rowTitle: (c) => c.reason,
  onSelect: (c) => showChange(c),
  empty: () => (rv.cmp?.result ? "No differences of the selected kinds." : ""),
});
function showChange(c) {
  const cmp = rv.cmp; if (!cmp) return;
  const side = (viewer, doc, page, box, reading, missing) => {
    if (page && box) viewer.load({ path: doc.path, name: doc.name, page, query: "", office: cmp.office, extra: [box], extraLabel: reading ? `Read as ${reading}` : "" });
    else if (page) viewer.load({ path: doc.path, name: doc.name, page, query: "", office: cmp.office });
    else viewer.clear(missing);
  };
  side(revOld, cmp.o, c.old_page, c.old_box, c.old, "This tag was not found on the old revision.");
  side(revNew, cmp.n, c.new_page, c.new_box, c.new, "This tag was not found on the new revision.");
}
$("revList").addEventListener("keydown", (e) => {
  const n = rv.groups.length; if (!n) return;
  let i = rv.groups.indexOf(rv.open);
  if (e.key === "ArrowDown") i = Math.min(n - 1, i + 1);
  else if (e.key === "ArrowUp") i = Math.max(0, i - 1);
  else if (e.key === "Enter" && rv.open) { e.preventDefault(); runCompare(); return; }
  else return;
  e.preventDefault();
  if (rv.groups[i] !== rv.open) openGroup(rv.groups[i]);
  $("revList").querySelector(".rev-item.open")?.scrollIntoView({ block: "nearest" });
});
$("revFilter").addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown") { e.preventDefault(); $("revList").focus(); if (!rv.open && rv.groups[0]) openGroup(rv.groups[0]); }
  if (e.key === "Escape") { $("revFilter").value = ""; loadRevisions(); }
});
$("revFilter").addEventListener("input", debounce(loadRevisions, 250));
$("revProject").onchange = loadRevisions;
initSeg("revSource", loadRevisions);
viewHooks.revisions = {
  show() {
    syncSeg("revSource", loadRevisions);
    if (!rv.gen) loadRevisions();
    $("sbCount").textContent = "Revisions";
  },
};

// =====================================================================
// Check readings (correction studio)
// =====================================================================
const CHK_PAGE = 20;
const ck = { offset: 0, total: 0, items: [], gen: 0 };
function chkPath() { return $("chkOnlyFile").checked && pv.hit && pv.hit.origin !== "office" ? pv.hit.path : null; }
function syncChkFile() {
  const h = pv.hit && pv.hit.origin !== "office" ? pv.hit : null;
  $("chkOnlyFile").disabled = !h;
  if (!h) $("chkOnlyFile").checked = false;
  $("chkOnlyFileLbl").textContent = h ? `Only ${h.name}` : "Only the file selected in Search (select one first)";
}
async function loadLow() {
  const gen = ++ck.gen;
  const box = $("chkList");
  box.replaceChildren(el("div", "dim pad", "Loading…"));
  try {
    const r = await call({ req: "low_confidence", path: chkPath(), limit: CHK_PAGE, offset: ck.offset });
    if (gen !== ck.gen) return;
    ck.items = r.items; ck.total = Number(r.total) || 0;
    renderLow();
  } catch (e) { if (gen === ck.gen) box.replaceChildren(el("div", "error pad", String(e))); }
}
function renderLow() {
  const box = $("chkList");
  box.replaceChildren();
  const from = ck.total ? ck.offset + 1 : 0, to = Math.min(ck.total, ck.offset + ck.items.length);
  $("chkPageInfo").textContent = ck.total ? `${fmtNum(from)}–${fmtNum(to)} of ${fmtNum(ck.total)}` : "";
  $("chkPrev").disabled = ck.offset <= 0;
  $("chkNext").disabled = ck.offset + CHK_PAGE >= ck.total;
  $("sbCount").textContent = `${fmtNum(ck.total)} low-confidence line${ck.total === 1 ? "" : "s"}`;
  if (!ck.items.length) { box.append(el("div", "dim pad", "No low-confidence readings to check.")); return; }
  const queue = [];
  for (const it of ck.items) {
    const card = el("div", "chk" + (it.corrected != null ? " done" : ""));
    const head = el("div", "chk-head");
    const nm = el("span", "ell", `${it.name} · page ${it.page}`); nm.title = it.display_path;
    const open = el("button", "small", "Open file"); open.type = "button"; open.onclick = () => openFile(it.display_path);
    head.append(nm, el("span", "grow"), confCell(it.conf), open);
    const img = el("div", "chk-img"); img.append(el("span", "dim", "Loading image…"));
    queue.push([it, img]);
    const read = el("dl", "kv");
    kvAdd(read, "OCR read", it.text, "mono big");
    if (it.alt) kvAdd(read, "Second reader", it.alt, "mono big");
    card.append(head, img, read);
    if (it.corrected != null) {
      const row = el("div", "chk-row");
      row.append(el("span", "pill exact", "Corrected"), el("span", "mono big", it.corrected));
      const undo = el("button", "small", "Undo"); undo.type = "button";
      undo.onclick = async () => {
        undo.disabled = true;
        try { await call({ req: "uncorrect", correction_id: it.correction_id }); toast("Correction removed; the OCR reading is used again"); loadLow(); }
        catch (e) { toast(String(e)); undo.disabled = false; }
      };
      row.append(el("span", "grow"), undo);
      card.append(row);
    } else {
      const row = el("div", "chk-row");
      const inp = el("input"); inp.type = "text"; inp.className = "mono grow-input"; inp.spellcheck = false;
      inp.placeholder = "Type the text exactly as printed"; inp.setAttribute("aria-label", "Text as printed on the drawing");
      inp.value = "";
      const save = el("button", "small primary", "Save correction"); save.type = "button";
      const doSave = async () => {
        const text = inp.value.trim();
        if (!text) { inp.focus(); toast("Type the text as printed first"); return; }
        save.disabled = true;
        try { await call({ req: "correct", line_id: it.line_id, text }); toast("Correction saved. The OCR reading is kept as well."); loadLow(); }
        catch (e) { toast(String(e)); save.disabled = false; }
      };
      save.onclick = doSave;
      inp.onkeydown = (e) => { if (e.key === "Enter") { e.preventDefault(); doSave(); } };
      const useOcr = el("button", "small", "Copy OCR text"); useOcr.type = "button"; useOcr.title = "Start from the OCR reading and edit it";
      useOcr.onclick = () => { inp.value = it.text; inp.focus(); };
      row.append(inp, useOcr, save);
      card.append(row);
    }
    box.append(card);
  }
  loadCrops(queue, ck.gen);
}
async function loadCrops(queue, gen) {
  // A few at a time so the reader stays responsive.
  const worker = async () => {
    while (queue.length && gen === ck.gen) {
      const [it, img] = queue.shift();
      const b = it.bbox;
      // A little context around the line; the box itself is unchanged.
      const padX = Math.max(0.01, b.w * 0.08), padY = Math.max(0.006, b.h * 0.6);
      const x = Math.max(0, b.x - padX), y = Math.max(0, b.y - padY);
      const bbox = { x, y, w: Math.min(1 - x, b.w + 2 * padX), h: Math.min(1 - y, b.h + 2 * padY) };
      try {
        const r = await call({ req: "crop", path: it.path, page: it.page, bbox, max_px: 900 });
        if (gen !== ck.gen) return;
        const im = el("img"); im.alt = "The line as printed on the drawing"; im.src = "data:image/png;base64," + r.png_base64;
        img.replaceChildren(im);
      } catch (e) { img.replaceChildren(el("span", "error", "Image not available: " + e)); }
    }
  };
  await Promise.all([worker(), worker(), worker()]);
}
$("chkPrev").onclick = () => { ck.offset = Math.max(0, ck.offset - CHK_PAGE); loadLow(); };
$("chkNext").onclick = () => { ck.offset += CHK_PAGE; loadLow(); };
$("chkReload").onclick = () => loadLow();
$("chkOnlyFile").onchange = () => { ck.offset = 0; loadLow(); };
viewHooks.check = { show() { syncChkFile(); ck.offset = 0; loadLow(); } };

// =====================================================================
// Monitor
// =====================================================================
let monTimer = 0;
async function loadMonitor() {
  try {
    const r = await call({ req: "monitor" });
    renderMonitor(r.monitor);
    $("monUpdated").textContent = "Updated " + new Date().toLocaleTimeString();
  } catch (e) { $("monUpdated").textContent = String(e); }
}
function statCard(label, value, note) {
  const c = el("div", "stat");
  c.append(el("div", "l", label), el("div", "v", value));
  if (note) c.append(el("div", "n", note));
  return c;
}
function section(title, ...children) {
  const s = el("section", "mon-sec");
  s.append(el("h3", null, title), ...children);
  return s;
}
function table(headers, rows, cls) {
  const t = el("table", "grid " + (cls || ""));
  t.append(trow(headers, "th"));
  for (const r of rows) t.append(trow(r));
  return t;
}
const SERVER_LABEL = { not_configured: "Not set up (Sleuth works on its own)", connected: "Connected", offline: "Offline", degraded: "Refusing this PC", syncing: "Connected, still reading its first pass" };
function renderMonitor(m) {
  const s = m.status || {};
  const body = $("monBody");
  const scroll = body.scrollTop;
  body.replaceChildren();
  const g1 = el("div", "stats wide");
  g1.append(
    statCard("Files indexed", fmtNum(s.files_indexed)),
    statCard("PDFs read", fmtNum(s.pdfs_read)),
    statCard("Waiting to be read", fmtNum(s.pdfs_waiting)),
    statCard("Could not be read", fmtNum(s.pdfs_failed)),
    statCard("Pages per hour", fmtNum(s.pages_per_hour)),
    statCard("OCR ms per page", fmtNum(m.ocr_millis_per_page)),
    statCard("Pages read", fmtNum(m.pages_total)),
    statCard("Database size", fmtSize(m.db_bytes)),
  );
  body.append(section("Reading", g1, (() => {
    const dl = el("dl", "kv");
    kvAdd(dl, "Reader", cap(s.reader_state || ""));
    if (s.current_file) kvAdd(dl, "Current file", s.current_file, "mono");
    kvAdd(dl, "OCR model", s.ocr_model || "");
    kvAdd(dl, "OCR runs on", s.ocr_backend || "");
    if (m.backend_note) kvAdd(dl, "Why", m.backend_note);
    kvAdd(dl, "Change journal", m.change_journal || "");
    kvAdd(dl, "Reader version", s.worker_version || "");
    return dl;
  })()));
  body.append(section("Pages by kind", table(["Kind", "Pages"], (m.pages_by_class || []).map(([k, n]) => [PAGE_CLASS[k] || k, fmtNum(n)]), "num-cols narrow")));
  const g2 = el("div", "stats wide");
  g2.append(
    statCard("Shared cache: found", fmtNum(m.cache_hits)),
    statCard("Shared cache: not found", fmtNum(m.cache_misses)),
    statCard("Waited for another PC", fmtNum(m.cache_claims_waited), "Files another PC was already reading"),
    statCard("Searches", fmtNum(m.searches)),
    statCard("Search time, typical", fmtNum(m.search_p50_ms) + " ms", "median (p50)"),
    statCard("Search time, slow", fmtNum(m.search_p95_ms) + " ms", "95th percentile"),
  );
  body.append(section("Cache and searching", g2, (() => {
    const c = s.cache || {};
    const p = el("p", "hint", `Shared cache: ${c.state === "ok" ? "working" : (c.state || "").replace("_", " ")}${c.path ? " · " + c.path : ""}${c.message && c.state !== "ok" ? " · " + c.message : ""}`);
    return p;
  })()));
  const g3 = el("div", "stats wide");
  g3.append(
    statCard("Distinct tags", fmtNum(m.tags)),
    statCard("Tag places", fmtNum(m.tag_occurrences)),
    statCard("Drawings with title-block details", fmtNum(m.drawings_with_metadata)),
    statCard("Corrections", fmtNum(m.corrections)),
  );
  body.append(section("Index contents", g3));
  body.append(section("Folders and drives", (m.roots || []).length
    ? table(["Location", "State", "Last complete pass", "Note"], m.roots.map(([root, st, ok, note]) => [root, st, fmtDateTime(ok), note]))
    : el("div", "dim", "None")));
  const errs = m.rule_errors || [];
  const errBox = el("div");
  if (!errs.length) errBox.append(el("div", "dim", "None. All tag and title-block rules are in use."));
  for (const e of errs) errBox.append(el("div", "error mono small-text", e));
  body.append(section("Rule errors", errBox));
  const srv = s.server || {};
  const sd = el("dl", "kv");
  kvAdd(sd, "State", SERVER_LABEL[srv.state] || srv.state || "");
  if (srv.message) kvAdd(sd, "Message", srv.message);
  if (srv.version) kvAdd(sd, "Server version", srv.version);
  if (srv.state && srv.state !== "not_configured") kvAdd(sd, "Last contact", fmtDateTime(srv.last_ok));
  if ((srv.roots || []).length) kvAdd(sd, "Folders", srv.roots.join("\n"), "mono pre");
  if ((srv.projects || []).length) kvAdd(sd, "Projects", srv.projects.join(", "));
  body.append(section("Office server", sd));
  body.append(section("Recent events", (m.events || []).length
    ? table(["When", "Level", "Message"], m.events.slice(0, 200).map(([t, lvl, msg]) => [fmtDateTime(t), lvl, msg]), "events")
    : el("div", "dim", "None")));
  body.scrollTop = scroll;
}
$("monRefresh").onclick = loadMonitor;
viewHooks.monitor = {
  show() { $("sbCount").textContent = "Monitor"; loadMonitor(); clearInterval(monTimer); monTimer = setInterval(loadMonitor, 5000); },
  hide() { clearInterval(monTimer); monTimer = 0; },
};
