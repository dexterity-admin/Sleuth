// Sleuth window, shared parts. Talks to sleuth-worker.exe through the Rust shell:
//   invoke("call", {req})           one request, one reply
//   invoke("start_search", {...})   streaming search; events arrive as "worker-event"
// Scripts load in order: core.js, search.js, views.js, app.js. They share the
// global scope (no modules, no framework).
"use strict";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(id);
const call = (req) => invoke("call", { req });

const PROTOCOL = 3; // 3 = Sleuth 1.1: office index, every file type, stage controls
const ROW_H = 30;
const VERIFY_BELOW = 0.8; // confidence under this is always marked "verify"
const CLASS_LABEL = { exact: "Exact", separators_ignored: "Separators ignored", uncertain: "Verify, OCR unsure", partial: "Partial, verify" };
const CLASS_PILL = { exact: "exact", separators_ignored: "sep", uncertain: "verify", partial: "partial" };
const CLASS_ORDER = { exact: 0, separators_ignored: 1, partial: 2, uncertain: 3 };
const SOURCE_LABEL = { file_name: "File name", text_layer: "PDF text", ocr: "OCR", pending: "Waiting to be read", verified: "Checked by a person", metadata: "Title block" };
// File types read inside (crates/sleuth-core/src/kinds.rs). "other" is found by name only.
const FILE_KIND = { pdf: "PDF", excel: "Excel", word: "Word", slides: "PowerPoint", email: "Email", text: "Text", dwg: "DWG", dgn: "DGN", image: "Image" };
const KIND_LONG = { pdf: "PDF", excel: "Excel workbooks", word: "Word documents", slides: "PowerPoint presentations", email: "Emails (.msg, .eml)", text: "Text and CSV files", dwg: "AutoCAD drawings (DWG, DXF)", dgn: "MicroStation drawings (DGN, PDS .i01)", image: "Scanned images (TIF, JPG, PNG), by OCR" };
const OPEN_IN = { excel: "Excel", word: "Word", slides: "PowerPoint", email: "Outlook", dwg: "AutoCAD", dgn: "MicroStation" };
const PAGE_WORD = { excel: "Sheet", slides: "Slide" };
const CATEGORIES = [["drawing", "Drawings"], ["document", "Documents"], ["spreadsheet", "Spreadsheets"], ["email", "Emails"], ["presentation", "Presentations"], ["other", "Other files"]];
const SPEED_LABEL = { auto: "Automatic", gentle: "Gentle (minimum only)", full: "Full speed" };
// Copy of the built-in categories in crates/sleuth-core/src/settings.rs; the
// list is extended with any category seen in the data or in custom rules.
const BUILTIN_CATEGORIES = ["line", "drawing", "instrument", "valve", "equipment", "support", "weld", "spec"];

const state = {
  settings: null,
  lastStatus: null,
  server: { state: "not_configured" },
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
  page: { offset: 0, more: false, deep: false, loadingMore: false },
  updateChannel: "stable",
  view_: "search",
  proto: 0,                 // the background reader's protocol; 0 until it has answered
  pcName: "",               // this PC as the office sees it (from Monitor)
  folded: 0,                // rows folded into another file type of the same document
};
/** The 1.1 background reader: office index, every file type, stage controls. */
function newReader() { return state.proto >= 3; }

// ---------- helpers ----------
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined && text !== null) e.textContent = text;
  return e;
}
function fmtDate(secs) {
  if (!secs) return "";
  const d = new Date(secs * 1000);
  return d.toLocaleDateString(undefined, { day: "2-digit", month: "short", year: "numeric" });
}
function fmtDateTime(secs) {
  if (!secs) return "never";
  const d = new Date(secs * 1000);
  return d.toLocaleString(undefined, { day: "2-digit", month: "short", year: "numeric", hour: "2-digit", minute: "2-digit" });
}
function fmtSize(n) {
  n = Number(n) || 0;
  if (n < 1024) return n + " B";
  if (n < 1048576) return (n / 1024).toFixed(0) + " KB";
  if (n < 1073741824) return (n / 1048576).toFixed(1) + " MB";
  return (n / 1073741824).toFixed(2) + " GB";
}
function fmtNum(n) { return Number(n || 0).toLocaleString(); }
function pct(c) { return c === null || c === undefined ? "" : Math.round(c * 100) + "%"; }
function folderOf(p) { const i = p.lastIndexOf("\\"); return i > 0 ? p.slice(0, i) : p; }
function baseName(p) { return p.split("\\").pop(); }
function extOf(name) { const i = name.lastIndexOf("."); return i > 0 ? name.slice(i + 1).toUpperCase().slice(0, 4) : ""; }
function isPdf(name) { return /\.pdf$/i.test(name); }
function normTag(s) { return String(s || "").toUpperCase().replace(/[^A-Z0-9]/g, ""); }
function toast(msg) { $("sbSearch").textContent = msg; }
function lsGet(k) { try { return localStorage.getItem(k); } catch { return null; } }
function lsSet(k, v) { try { if (v == null) localStorage.removeItem(k); else localStorage.setItem(k, v); } catch { /* private mode */ } }
function cap(s) { return s ? s[0].toUpperCase() + s.slice(1) : ""; }
/** Time left, for people: "less than a minute", "about 12 min". An estimate, so "about", except under a minute. */
function fmtLeft(secs) { return Math.round(Number(secs) || 0) < 60 ? fmtDur(secs) : `about ${fmtDur(secs)}`; }
/** A duration for people: "less than a minute", "12 min", "3 h 20 min", "2 days 4 h". */
function fmtDur(secs) {
  secs = Math.max(0, Math.round(Number(secs) || 0));
  if (secs < 60) return "less than a minute";
  const m = Math.round(secs / 60);
  if (m < 60) return `${m} min`;
  const h = Math.floor(m / 60), mm = m % 60;
  if (h < 24) return mm ? `${h} h ${mm} min` : `${h} h`;
  const d = Math.floor(h / 24), hh = h % 24;
  return `${d} day${d === 1 ? "" : "s"}` + (hh ? ` ${hh} h` : "");
}
const nowSecs = () => Math.floor(Date.now() / 1000);
/** "just now", "12 min ago", "3 h ago"; older than a week shows the date. */
function fmtAgo(secs) {
  if (!secs) return "never";
  const d = nowSecs() - secs;
  if (d < 60) return "just now";
  if (d > 7 * 86400) return "on " + fmtDate(secs);
  return fmtDur(d) + " ago";
}
function fmtIn(secs) {
  if (!secs) return "";
  const d = secs - nowSecs();
  return d < 60 ? "in a moment" : "in " + fmtDur(d);
}
const plural = (n, one, many) => `${fmtNum(n)} ${Number(n) === 1 ? one : (many || one + "s")}`;
/** "14:35" today, otherwise the date and time. */
function fmtClock(secs) {
  const d = new Date(secs * 1000);
  const t = d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
  return d.toDateString() === new Date().toDateString() ? t : `${t} on ${fmtDate(secs)}`;
}
/** The type label of a result row: "Excel", "DGN"; the extension for types not read inside. */
function kindLabel(f) { return FILE_KIND[f.kind] || extOf(f.name || "") || "FILE"; }
/** Whether the file's text is shown as text (no page image): every read type except PDF. */
function readsAsText(f) { return !!f.kind && f.kind !== "pdf" && f.kind !== "other"; }
function sourceLabel(h) { return h.source === "text_layer" && readsAsText(h) ? "Text in the file" : (SOURCE_LABEL[h.source] || h.source); }
function isThisPc(name) { return !name || (!!state.pcName && name.toLowerCase() === state.pcName.toLowerCase()); }
/** "Z:\ (\\server\Project)" -> "Z:\" for the short forms. */
function shortRoot(r) { const d = r.display || r.root || ""; const i = d.indexOf(" ("); return i > 0 ? d.slice(0, i) : d; }

// ---------- indexing progress ----------
// One reading of the worker's status, shared by the status strip, the welcome
// card, the search banner, the reading-folder list and Monitor. Every newer
// field may be missing (an older background reader): fall back quietly.
const ROOT_STATE = {
  waiting: ["idle", "Waiting its turn"],
  walking: ["run", "Listing folders"],
  done: ["ok", "Done"],
  partial: ["warn", "Partly listed"],
  offline: ["off", "Offline"],
};
const SCOPE_NOTE = {
  "not mapped": "This drive letter is not mapped on this PC, so nothing in it can be read. Map the drive, or remove it and add the folder by its network path.",
  offline: "Cannot be reached right now. Reading continues when it is back.",
  "not indexed yet": "Its files have not been listed yet (stage 1). Its documents are queued as soon as the listing reaches it.",
};
function progressModel(s) {
  s = s || {};
  const ix = s.index && typeof s.index === "object" && s.index.phase ? s.index : null;
  const roots = Array.isArray(ix?.roots) ? ix.roots : [];
  // Paused by the user on this PC (1.1). "loading" = the file list is being opened.
  const walkPaused = s.walk_paused === true || (s.walk_paused === undefined && ix?.phase === "paused");
  const loading = !!ix && ix.phase === "loading";
  const phase = !ix ? "unknown" : ix.phase === "up to date" || ix.phase === "paused" ? "done" : ix.phase === "starting" || loading ? "starting" : "walking";
  const scopes = Array.isArray(s.scopes) && s.scopes.length ? s.scopes : null;
  const sum = (k) => scopes.reduce((a, x) => a + (Number(x[k]) || 0), 0);
  let read, waiting, failed, total;
  if (scopes) { read = sum("read"); waiting = sum("waiting"); failed = sum("failed"); total = sum("pdfs"); }
  else { read = Number(s.pdfs_read) || 0; waiting = Number(s.pdfs_waiting) || 0; failed = Number(s.pdfs_failed) || 0; total = read + waiting + failed; }
  if (total < read + waiting + failed) total = read + waiting + failed;
  const rs = String(s.reader_state || "").toLowerCase();
  const readPaused = s.read_paused === true;
  return {
    s, ix, roots, phase, scopes, loading, walkPaused, readPaused,
    walking: !walkPaused && (phase === "walking" || phase === "starting"),
    firstWalk: !!ix?.first_walk,
    // A share is listed by one PC for the whole office: ours, or another's.
    active: roots.filter((r) => r.state === "walking" && isThisPc(r.by)).map(shortRoot),
    others: roots.filter((r) => r.state === "walking" && !isThisPc(r.by)),
    offline: roots.filter((r) => r.state === "offline"),
    partial: roots.filter((r) => r.state === "partial"),
    scopeCount: scopes ? scopes.length : (state.settings?.content_scopes || []).length,
    // "not indexed yet" is expected while stage 1 runs; only the others need the user.
    scopeProblems: (scopes || []).filter((x) => x.note && x.note !== "ok" && x.note !== "not indexed yet"),
    read, waiting, failed, total,
    reading: rs.startsWith("reading") && !readPaused,
    paused: rs.startsWith("paused"),
    readDone: waiting === 0 && !rs.startsWith("reading"),
    speed: SPEED_LABEL[s.speed] ? s.speed : "",
    speedUntil: s.speed === "full" && Number(s.speed_until) > nowSecs() ? Number(s.speed_until) : 0,
  };
}
/** "Z:\ is being listed by PC-07", or "" when no other PC is listing a drive. */
function listedElsewhere(p) {
  const o = p.others;
  if (!o.length) return "";
  const pcs = [...new Set(o.map((r) => r.by))];
  if (pcs.length > 1) return `${fmtNum(o.length)} drives are being listed by other PCs`;
  const names = o.map(shortRoot);
  if (names.length > 3) return `${fmtNum(names.length)} drives are being listed by ${pcs[0]}`;
  return (names.length === 1 ? `${names[0]} is` : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]} are`) + ` being listed by ${pcs[0]}`;
}
/** The reading speed in words, "" for automatic: "Full speed until 14:35". */
function speedNote(p) {
  if (p.speed === "full") return p.speedUntil ? `Full speed until ${fmtClock(p.speedUntil)}` : "Full speed";
  return p.speed === "gentle" ? "Gentle speed" : "";
}
/** Stage 1, "Finding files": {cls, state, text, title, bar} (state: a word shown before the text, "Paused"; bar: null = moving, 0..1 = filled). */
function stageFind(p) {
  const s = p.s, ix = p.ix;
  const elsewhere = listedElsewhere(p);
  const elsewhereTitle = elsewhere ? "One PC lists each network drive for the whole office. Its files show here as they are found." : "";
  if (p.phase === "unknown") {
    return { cls: "idle", text: `${plural(s.files_indexed, "file")} in the index`, title: "This background reader does not report its listing progress. Update Sleuth on this PC to see it.", bar: 1 };
  }
  if (p.walkPaused) {
    const parts = ["on this PC", `${plural(s.files_indexed, "file")} in the index`];
    if (ix.last_complete) parts.push(`last checked ${fmtAgo(ix.last_complete)}`);
    if (elsewhere) parts.push(elsewhere);
    return { cls: "paused", state: "Paused", text: parts.join(" · "), title: "Finding files is paused on this PC until you resume it. Other PCs carry on.", bar: ix.last_complete ? 1 : 0 };
  }
  if (p.loading) return { cls: "run", text: "Opening the file list…", title: "", bar: null };
  if (p.phase === "starting") return { cls: "run", text: "Starting: getting the list of drives…", title: "", bar: null };
  if (p.walking) {
    const parts = [];
    if (p.active.length) parts.push(`Listing ${p.active.slice(0, 3).join(", ")}${p.active.length > 3 ? ` and ${p.active.length - 3} more` : ""}`);
    else if (!elsewhere) parts.push("Listing folders");
    if (elsewhere) parts.push(elsewhere);
    parts.push(`${plural(ix.folders_listed, "folder")} listed` + (ix.folders_pending ? `, ${fmtNum(ix.folders_pending)} still to list` : ""));
    parts.push(`${plural(ix.files_seen, "file")} found`);
    if (ix.pass_started) parts.push(`running ${fmtDur(nowSecs() - ix.pass_started)}`);
    const title = (p.firstWalk ? "First listing of the drives: search results stay incomplete until it finishes." : "Checking the drives for new, moved and deleted files.") +
      (ix.last_complete ? `\nLast complete listing ${fmtAgo(ix.last_complete)}.` : "") + (elsewhereTitle ? `\n${elsewhereTitle}` : "");
    return { cls: "run", text: parts.join(" · "), title, bar: null };
  }
  const warn = [];
  if (p.offline.length) warn.push(`${plural(p.offline.length, "drive")} offline`);
  if (p.partial.length) warn.push(`${plural(p.partial.length, "drive")} partly listed`);
  if (elsewhere) {
    const text = [elsewhere, `${plural(s.files_indexed, "file")} in the index so far`, ...warn].join(" · ");
    return { cls: "run", text, title: elsewhereTitle, bar: null };
  }
  const text = [`Up to date · ${plural(s.files_indexed, "file")}`, `checked ${fmtAgo(ix.last_complete)}`, ...warn].join(" · ");
  return { cls: warn.length ? "warn" : "ok", text, title: ix.next_walk ? `Next check ${fmtIn(ix.next_walk)}.` : "", bar: 1 };
}
/** Stage 2, "Reading documents". */
function stageRead(p) {
  const s = p.s;
  const progress = `${fmtNum(p.read)} of ${fmtNum(p.total)} read`;
  const tail = [];
  if (p.failed) tail.push(`${fmtNum(p.failed)} could not be read`);
  if (p.scopeProblems.length) tail.push(`${plural(p.scopeProblems.length, "folder")} need${p.scopeProblems.length === 1 ? "s" : ""} attention`);
  const bar = p.total ? p.read / p.total : 0;
  const warnIf = (c) => (p.scopeProblems.length ? "warn" : c);
  if (!p.scopeCount) {
    return { cls: "warn", text: "No reading folders chosen: documents are found by file name only", title: "Add your project folders under Folders being read on the Search page so the text inside their drawings and documents can be searched.", bar: 0 };
  }
  if (p.readPaused) {
    const parts = ["on this PC", progress];
    if (p.waiting) parts.push(`${fmtNum(p.waiting)} waiting`);
    return { cls: "paused", state: "Paused", text: [...parts, ...tail].join(" · "), title: "Reading is paused on this PC until you resume it. Other PCs carry on.", bar };
  }
  if (p.reading) {
    // Most useful first: the strip cuts the end off on a narrow window.
    const parts = [progress];
    const speed = speedNote(p);
    if (speed) parts.push(speed);
    if (s.ocr_waiting > 0) parts.push(`${fmtNum(s.ocr_waiting)} waiting for OCR`);
    else if (s.ocr_waiting === undefined) parts.push(`${fmtNum(p.waiting)} waiting`);
    parts.push(...tail);
    if (s.docs_per_hour > 0) parts.push(`${plural(s.docs_per_hour, "document")} an hour`);
    else if (s.docs_per_hour === undefined && s.pages_per_hour) parts.push(`${fmtNum(s.pages_per_hour)} pages/h`);
    if (s.reading_eta_secs > 0) parts.push(`${fmtLeft(s.reading_eta_secs)} left`);
    return { cls: warnIf("run"), text: parts.join(" · "), title: (s.current_file ? `Now reading ${s.current_file}` : "") + (s.reader_note ? `\n${s.reader_note}` : ""), bar };
  }
  if (p.waiting > 0) {
    const why = s.reader_note || (s.reader_state ? cap(s.reader_state) : "Waiting");
    // "paused: …" (not by the user) and "stopped" are the reader's own trouble: say so in the warning colour.
    return { cls: p.paused || /^stopped/i.test(s.reader_state || "") ? "warn" : warnIf("idle"), text: [cap(why), progress, `${fmtNum(p.waiting)} waiting`, ...tail].join(" · "), title: s.reader_note || "", bar };
  }
  if (!p.total) {
    if (p.walking) return { cls: warnIf("idle"), text: [cap(s.reader_note || "waiting for the file list to finish"), "no documents queued yet", ...tail].join(" · "), title: "Documents in your reading folders are queued as stage 1 lists them.", bar: 0 };
    return { cls: "warn", text: ["No documents found in your reading folders", ...tail].join(" · "), title: "", bar: 0 };
  }
  if (p.walking) return { cls: warnIf("idle"), text: [`All ${fmtNum(p.total)} documents found so far read`, "more are queued as stage 1 lists folders", ...tail].join(" · "), title: "", bar: 1 };
  return { cls: warnIf("ok"), text: [`All ${fmtNum(p.total)} documents in your reading folders read`, ...tail].join(" · "), title: "", bar: 1 };
}
/** The state word of a stage ("Paused") as a pill, or nothing. */
function stateWord(st) { return st.state ? el("span", "pill st-" + st.cls, st.state) : ""; }

// ---------- redrawing live panels ----------
// Monitor and Settings redraw their stage blocks from every status (2 s while
// working). A click must not be lost to a redraw between press and release,
// and the keyboard focus must stay on the button it was on (data-key).
function holdWhilePressed(box) {
  box.addEventListener("pointerdown", () => { box.dataset.held = "1"; });
  window.addEventListener("pointerup", () => { delete box.dataset.held; });
  window.addEventListener("pointercancel", () => { delete box.dataset.held; });
}
function redrawKeeping(box, draw) {
  if (box.dataset.held) return;
  const key = box.contains(document.activeElement) ? document.activeElement.dataset.key : null;
  draw();
  if (key) [...box.querySelectorAll("[data-key]")].find((b) => b.dataset.key === key)?.focus();
}

function debounce(fn, ms) { let t; return (...a) => { clearTimeout(t); t = setTimeout(() => fn(...a), ms); }; }
async function copyText(t) {
  try { await navigator.clipboard.writeText(t); toast("Copied"); }
  catch { toast("Could not copy"); }
}
async function openFile(path) { try { await invoke("open_path", { path }); } catch (e) { toast(String(e)); } }
async function revealFile(path) { try { await invoke("reveal_path", { path }); } catch (e) { toast(String(e)); } }
/** A confidence value with a visible "verify" mark when it is under 80%. */
function confCell(c, src) {
  const d = el("span", "conf");
  if (c === null || c === undefined) return d;
  d.append(el("span", "num", pct(c)));
  if (c < VERIFY_BELOW && src !== "verified") { d.append(" ", el("span", "pill verify", "verify")); d.title = "Read with low confidence: check the drawing"; }
  return d;
}
function kvAdd(dl, k, v, cls) {
  const dd = el("dd", cls);
  if (v instanceof Node) dd.append(v); else dd.textContent = v ?? "";
  dl.append(el("dt", null, k), dd);
}
function officeConnected() { return state.server && (state.server.state === "connected" || state.server.state === "syncing"); }
function projectNames() { return (state.settings?.projects || []).map((p) => p.name).filter(Boolean); }
function fillProjectSelect(sel, allLabel) {
  const cur = sel.value;
  sel.replaceChildren(new Option(allLabel, ""));
  const names = [...new Set([...projectNames(), ...(officeConnected() ? state.server.projects || [] : [])])];
  for (const n of names) sel.append(new Option(n, n));
  sel.value = names.includes(cur) ? cur : "";
}

// ---------- confirm dialog ----------
function confirmBox(title, body, yesLabel, noLabel) {
  return new Promise((resolve) => {
    $("cfTitle").textContent = title;
    const b = $("cfBody"); b.replaceChildren();
    for (const para of [].concat(body)) b.append(el("p", null, para));
    $("cfYes").textContent = yesLabel || "OK";
    $("cfNo").textContent = noLabel || "Cancel";
    const dlg = $("confirmDlg");
    const done = (v) => { dlg.close(); resolve(v); };
    $("cfYes").onclick = () => done(true);
    $("cfNo").onclick = () => done(false);
    dlg.oncancel = () => resolve(false);
    dlg.showModal();
  });
}

// ---------- export ----------
async function exportTable(format, fileName, sheet, headers, rows) {
  try {
    const saved = await invoke("export_table", { format, fileName, sheet, headers, rows });
    if (saved) toast(`Saved ${rows.length.toLocaleString()} rows to ${saved}`);
  } catch (e) { toast("Export failed: " + e); }
}

// ---------- views ----------
const viewHooks = {}; // name -> {show(), hide()}
function showView(name) {
  if (state.view_ === name) return;
  const prev = state.view_;
  state.view_ = name;
  for (const b of document.querySelectorAll(".tabs button")) { const on = b.dataset.view === name; b.classList.toggle("on", on); b.setAttribute("aria-selected", on); }
  for (const v of document.querySelectorAll("main.view")) v.hidden = v.id !== "view-" + name;
  $("searchBar").hidden = name !== "search";
  viewHooks[prev]?.hide?.();
  viewHooks[name]?.show?.();
}
for (const b of document.querySelectorAll(".tabs button")) b.onclick = () => showView(b.dataset.view);

// ---------- virtual table ----------
// A fixed-row-height list that only builds the rows in view, so thousands of
// rows stay fast. `columns`: [{key, label, cls, width, render(row) -> Node|string}].
class VTable {
  constructor(host, opts) {
    this.o = Object.assign({ rowH: ROW_H, columns: [], count: () => 0, row: () => null, onSelect: null, onOpen: null, onNeed: null, empty: "" }, opts);
    this.sel = -1;
    host.classList.add("vt");
    const tpl = this.o.columns.map((c) => c.width || "minmax(0,1fr)").join(" ");
    this.head = el("div", "vt-head");
    this.head.style.gridTemplateColumns = tpl;
    for (const c of this.o.columns) {
      const h = el("div", c.cls || "", c.label);
      if (c.title) h.title = c.title;
      this.head.append(h);
    }
    this.body = el("div", "vt-body");
    this.body.tabIndex = 0;
    this.body.setAttribute("role", "grid");
    this.spacer = el("div", "vt-spacer");
    this.emptyEl = el("div", "vt-empty");
    this.body.append(this.spacer);
    host.replaceChildren(this.head, this.body, this.emptyEl);
    this.tpl = tpl;
    this.body.addEventListener("scroll", () => this.render());
    window.addEventListener("resize", () => this.render());
    this.body.addEventListener("keydown", (e) => {
      const n = this.o.count();
      if (!n) return;
      if (e.key === "ArrowDown") { e.preventDefault(); this.select(Math.min(n - 1, this.sel + 1)); }
      else if (e.key === "ArrowUp") { e.preventDefault(); this.select(Math.max(0, this.sel - 1)); }
      else if (e.key === "PageDown") { e.preventDefault(); this.select(Math.min(n - 1, this.sel + 10)); }
      else if (e.key === "PageUp") { e.preventDefault(); this.select(Math.max(0, this.sel - 10)); }
      else if (e.key === "Home") { e.preventDefault(); this.select(0); }
      else if (e.key === "End") { e.preventDefault(); this.select(n - 1); }
      else if (e.key === "Enter" && this.sel >= 0) { e.preventDefault(); this.o.onOpen?.(this.o.row(this.sel), this.sel); }
    });
  }
  refresh(keepSel) {
    if (!keepSel) this.sel = -1;
    const n = this.o.count();
    this.spacer.style.height = n * this.o.rowH + "px";
    this.emptyEl.textContent = n ? "" : (typeof this.o.empty === "function" ? this.o.empty() : this.o.empty);
    this.emptyEl.hidden = !!n;
    this.render();
  }
  render() {
    const n = this.o.count(), H = this.o.rowH;
    const top = this.body.scrollTop, h = this.body.clientHeight || 400;
    const first = Math.max(0, Math.floor(top / H) - 5);
    const last = Math.min(n, Math.ceil((top + h) / H) + 5);
    const frag = document.createDocumentFragment();
    let missing = -1;
    for (let i = first; i < last; i++) {
      const row = this.o.row(i);
      const r = el("div", "vt-row" + (i === this.sel ? " on" : ""));
      r.style.top = i * H + "px"; r.style.height = H + "px";
      r.style.gridTemplateColumns = this.tpl;
      r.setAttribute("role", "row");
      if (!row) { if (missing < 0) missing = i; r.append(el("div", "dim", "Loading…")); frag.append(r); continue; }
      if (this.o.rowClass) r.className += " " + (this.o.rowClass(row) || "");
      if (this.o.rowTitle) r.title = this.o.rowTitle(row) || "";
      for (const c of this.o.columns) {
        const v = c.render ? c.render(row) : row[c.key];
        const d = el("div", c.cls || "");
        if (v instanceof Node) d.append(v); else { d.textContent = v ?? ""; d.title = v ?? ""; }
        r.append(d);
      }
      r.onclick = () => this.select(i);
      r.ondblclick = () => this.o.onOpen?.(row, i);
      frag.append(r);
    }
    this.spacer.replaceChildren(frag);
    if (missing >= 0) this.o.onNeed?.(missing);
  }
  select(i, noCallback) {
    const n = this.o.count();
    if (i < 0 || i >= n) return;
    this.sel = i;
    const H = this.o.rowH, y = i * H, b = this.body;
    if (y < b.scrollTop) b.scrollTop = y;
    else if (y + H > b.scrollTop + b.clientHeight) b.scrollTop = y + H - b.clientHeight;
    this.render();
    const row = this.o.row(i);
    if (row && !noCallback) this.o.onSelect?.(row, i);
  }
  focus() { this.body.focus(); }
}

// ---------- page viewer ----------
// One rendered page with its marks, mark stepping, zoom and page buttons.
// Used by the search preview, the tags view and both sides of a comparison.
class Viewer {
  constructor(host, opts) {
    this.o = Object.assign({ height: "46vh", emptyText: "" }, opts);
    this.doc = null; this.page = 1; this.pages = 0; this.boxes = []; this.cur = 0; this.zoom = false; this.seq = 0;
    const tools = el("div", "pv-tools");
    this.prevHit = el("button", "small", "‹ Mark"); this.prevHit.type = "button"; this.prevHit.title = "Previous mark";
    this.hitInfo = el("span");
    this.nextHit = el("button", "small", "Mark ›"); this.nextHit.type = "button"; this.nextHit.title = "Next mark";
    this.zoomBtn = el("button", "small", "Zoom"); this.zoomBtn.type = "button"; this.zoomBtn.title = "Zoom to the mark / fit page";
    tools.append(this.prevHit, this.hitInfo, this.nextHit, el("span", "grow"), this.zoomBtn);
    this.stage = el("div", "pv-stage"); this.stage.style.height = this.o.height;
    this.canvas = el("div", "pv-canvas");
    this.img = el("img"); this.img.alt = "";
    this.canvas.append(this.img); this.stage.append(this.canvas);
    this.msg = el("div", "pv-msg", this.o.emptyText); this.stage.append(this.msg);
    const pages = el("div", "pv-pages");
    this.prevPage = el("button", "small", "‹"); this.prevPage.type = "button"; this.prevPage.setAttribute("aria-label", "Previous page");
    this.pageInfo = el("span");
    this.nextPage = el("button", "small", "›"); this.nextPage.type = "button"; this.nextPage.setAttribute("aria-label", "Next page");
    pages.append(this.prevPage, this.pageInfo, this.nextPage);
    host.classList.add("viewer");
    host.replaceChildren(tools, this.stage, pages);
    this.prevHit.onclick = () => this.step(-1);
    this.nextHit.onclick = () => this.step(1);
    this.zoomBtn.onclick = () => this.applyZoom(!this.zoom);
    this.prevPage.onclick = () => { if (this.doc && this.page > 1) this.load(this.doc, this.page - 1); };
    this.nextPage.onclick = () => { if (this.doc && this.page < this.pages) this.load(this.doc, this.page + 1); };
  }
  clear(text) {
    this.doc = null; this.seq++;
    this.img.removeAttribute("src");
    this.canvas.querySelectorAll(".mark").forEach((m) => m.remove());
    this.msg.textContent = text || this.o.emptyText; this.msg.hidden = !this.msg.textContent;
    this.hitInfo.textContent = ""; this.pageInfo.textContent = "";
  }
  /** doc: {path, name, kind, page, query, tag, office, extra:[NormBox], extraLabel}. */
  async load(doc, page) {
    const seq = ++this.seq;
    this.doc = doc;
    this.canvas.querySelectorAll(".mark").forEach((m) => m.remove());
    this.msg.hidden = true;
    if (!isPdf(doc.name || doc.path)) {
      this.img.removeAttribute("src");
      this.hitInfo.textContent = "";
      this.pageInfo.textContent = doc.kind === "other" ? "Found by its file name. Sleuth does not read inside this type of file." : "Preview is shown for PDFs";
      return;
    }
    const pg = page || doc.page || 1;
    this.page = pg;
    let loading = this.stage.querySelector(".pv-loading");
    if (!loading) { loading = el("div", "pv-loading", "Loading page…"); this.stage.append(loading); }
    try {
      const req = { req: "preview", path: doc.path, page: pg, query: doc.query || "", max_px: 3000, office: !!doc.office };
      if (doc.tag) req.tag = doc.tag;
      const r = await call(req);
      if (seq !== this.seq) return;
      this.pages = r.pages;
      // Boxes we were given (e.g. a comparison's) come first so stepping starts there.
      const extra = (doc.extra && pg === (doc.page || 1)) ? doc.extra : [];
      this.boxes = [...extra.map((b) => ({ ...b, extra: true })), ...r.boxes];
      this.cur = 0;
      this.img.src = "data:image/png;base64," + r.png_base64;
      for (const [i, b] of this.boxes.entries()) {
        const m = el("div", "mark" + (b.extra ? " given" : "") + (i === 0 ? " cur" : ""));
        // Pad the box a little so a thin line of text is clearly circled.
        Object.assign(m.style, { left: `calc(${b.x * 100}% - 4px)`, top: `calc(${b.y * 100}% - 4px)`, width: `calc(${b.w * 100}% + 8px)`, height: `calc(${b.h * 100}% + 8px)` });
        if (b.extra && doc.extraLabel) m.title = doc.extraLabel;
        this.canvas.append(m);
      }
      this.pageInfo.textContent = `Page ${pg} of ${r.pages}`;
      this.hitInfo.textContent = this.boxes.length ? `Mark 1 of ${this.boxes.length}` : (r.source === "pending" ? "Not read yet" : "No mark on this page");
      this.applyZoom(this.boxes.length > 0);
      this.o.onLoaded?.(r);
    } catch (e) {
      if (seq !== this.seq) return;
      this.img.removeAttribute("src");
      this.pageInfo.textContent = String(e);
    } finally { if (seq === this.seq) loading.remove(); }
  }
  applyZoom(zoom) {
    this.zoom = zoom;
    const b = this.boxes[this.cur];
    if (zoom && b) {
      // Show the mark about a third of the stage wide, centred.
      const scale = Math.min(8, Math.max(1.5, 0.33 / Math.max(b.w, 0.01)));
      this.canvas.style.width = scale * 100 + "%";
      requestAnimationFrame(() => {
        const W = this.canvas.clientWidth, H = this.canvas.clientHeight;
        this.stage.scrollLeft = (b.x + b.w / 2) * W - this.stage.clientWidth / 2;
        this.stage.scrollTop = (b.y + b.h / 2) * H - this.stage.clientHeight / 2;
      });
    } else {
      this.canvas.style.width = "100%";
      this.stage.scrollLeft = 0; this.stage.scrollTop = 0;
    }
    this.canvas.querySelectorAll(".mark").forEach((m, i) => m.classList.toggle("cur", i === this.cur));
    this.zoomBtn.textContent = zoom ? "Fit page" : "Zoom to mark";
  }
  step(d) {
    if (!this.boxes.length) return;
    this.cur = (this.cur + d + this.boxes.length) % this.boxes.length;
    this.hitInfo.textContent = `Mark ${this.cur + 1} of ${this.boxes.length}`;
    this.applyZoom(true);
  }
}

// ---------- text viewer ----------
// The text read from a file that has no page image (spreadsheet, drawing
// model, email, scanned image…), one sheet, slide or page at a time, with the
// matching lines marked and match stepping. Used by the search preview.
class TextViewer {
  constructor(host, opts) {
    this.o = Object.assign({ height: "46vh" }, opts);
    this.doc = null; this.page = 1; this.pages = 0; this.hits = []; this.cur = 0; this.seq = 0; this.retry = 0;
    const tools = el("div", "pv-tools");
    this.prevHit = el("button", "small", "‹ Match"); this.prevHit.type = "button"; this.prevHit.title = "Previous matching line";
    this.hitInfo = el("span");
    this.nextHit = el("button", "small", "Match ›"); this.nextHit.type = "button"; this.nextHit.title = "Next matching line";
    tools.append(this.prevHit, this.hitInfo, this.nextHit, el("span", "grow"));
    this.stage = el("div", "pv-stage tv-stage"); this.stage.style.height = this.o.height; this.stage.tabIndex = 0;
    this.stage.setAttribute("aria-label", "Text read from the file");
    this.label = el("h4", "tv-label"); this.lines = el("div", "tv-lines"); this.note = el("div", "tv-note");
    this.msg = el("div", "pv-msg");
    this.stage.append(this.label, this.lines, this.note, this.msg);
    const pages = el("div", "pv-pages");
    this.prevPage = el("button", "small", "‹"); this.prevPage.type = "button";
    this.pageInfo = el("span");
    this.nextPage = el("button", "small", "›"); this.nextPage.type = "button";
    pages.append(this.prevPage, this.pageInfo, this.nextPage);
    host.classList.add("viewer");
    host.replaceChildren(tools, this.stage, pages);
    this.prevHit.onclick = () => this.step(-1);
    this.nextHit.onclick = () => this.step(1);
    this.prevPage.onclick = () => { if (this.doc && this.page > 1) this.load(this.doc, this.page - 1); };
    this.nextPage.onclick = () => { if (this.doc && this.page < this.pages) this.load(this.doc, this.page + 1); };
    this.clear();
  }
  clear(text) {
    this.doc = null; this.seq++; clearTimeout(this.retry);
    this.lines.replaceChildren(); this.hits = [];
    this.label.hidden = true; this.note.hidden = true;
    this.say(text || "");
    this.hitInfo.textContent = ""; this.pageInfo.textContent = "";
    this.steppers(false, false);
  }
  say(...what) { this.msg.replaceChildren(...what); this.msg.hidden = !this.msg.childNodes.length || !this.msg.textContent; }
  steppers(hits, pages) {
    this.prevHit.hidden = this.nextHit.hidden = !hits;
    this.prevPage.hidden = this.nextPage.hidden = !pages;
  }
  ask(doc, page) {
    const req = { req: "text_preview", path: doc.path, page: page || doc.page || 1, query: doc.query || "" };
    if (doc.tag) req.tag = doc.tag;
    return call(req);
  }
  /** doc: {path, name, kind, page, query, tag}. */
  async load(doc, page) {
    const seq = ++this.seq;
    clearTimeout(this.retry);
    this.doc = doc;
    let loading = this.stage.querySelector(".pv-loading");
    if (!loading) { loading = el("div", "pv-loading", "Loading the text…"); this.stage.append(loading); }
    try {
      const r = await this.ask(doc, page);
      if (seq === this.seq) this.show(r, doc);
    } catch (e) {
      if (seq !== this.seq) return;
      this.lines.replaceChildren(); this.label.hidden = true; this.note.hidden = true;
      this.hitInfo.textContent = ""; this.pageInfo.textContent = ""; this.steppers(false, false);
      this.say(el("span", "error", String(e)));
    } finally { if (seq === this.seq) loading.remove(); }
  }
  show(r, doc) {
    const kind = r.kind && r.kind !== "other" ? r.kind : doc.kind;
    const word = PAGE_WORD[kind] || "Page";
    this.lines.replaceChildren(); this.hits = []; this.cur = 0;
    this.say(); this.note.hidden = true;
    this.label.textContent = r.label || ""; this.label.hidden = !r.label;
    if (!r.pages || r.source === "pending") { this.pending(doc); return; }
    this.pages = r.pages; this.page = r.page || 1;
    const frag = document.createDocumentFragment();
    for (const l of r.lines || []) {
      const low = l.conf < VERIFY_BELOW;
      const d = el("div", "tv-line" + (l.hit ? " hit" : "") + (low ? " low" : ""), l.text);
      if (low) d.title = `Read with low confidence (${pct(l.conf)}): check the file`;
      if (l.hit) this.hits.push(d);
      frag.append(d);
    }
    this.lines.append(frag);
    const many = this.pages > 1;
    if (!(r.lines || []).length) this.say(many ? `No text was read on this ${word.toLowerCase()}.` : "No text was read from this file.");
    if (r.truncated) {
      this.note.textContent = `Showing the first ${fmtNum(r.lines.length)} lines${many ? ` of this ${word.toLowerCase()}` : ""}. Open the file to see the rest.`;
      this.note.hidden = false;
    }
    this.pageInfo.textContent = many ? `${word} ${fmtNum(this.page)} of ${fmtNum(this.pages)}` : "";
    this.prevPage.setAttribute("aria-label", `Previous ${word.toLowerCase()}`); this.nextPage.setAttribute("aria-label", `Next ${word.toLowerCase()}`);
    this.prevPage.disabled = this.page <= 1; this.nextPage.disabled = this.page >= this.pages;
    this.steppers(this.hits.length > 1, many);
    this.noHit = many ? `No match on this ${word.toLowerCase()}` : "No matching line";
    this.mark();
    this.o.onLoaded?.(r);
  }
  /** Not read yet: say so, offer to read it first, then look again by itself. */
  pending(doc) {
    this.pages = 0; this.steppers(false, false);
    this.hitInfo.textContent = "Not read yet"; this.pageInfo.textContent = "";
    const box = el("div", "tv-pending");
    if ((state.settings?.kinds_not_read || []).includes(doc.kind)) {
      box.append(`Not read: ${KIND_LONG[doc.kind] || "files of this type"} are found by file name only on this PC. To read them, tick the type under Settings, File types read inside.`);
    } else {
      const b = el("button", "small primary", "Read now"); b.type = "button";
      b.title = "Read this file before the others that are waiting";
      b.onclick = async () => {
        b.disabled = true;
        try { await call({ req: "read_now", path: doc.path }); b.textContent = "Queued"; toast("Queued for reading first"); this.lookAgain(doc, 0); }
        catch (e) { toast(String(e)); b.disabled = false; }
      };
      box.append("Not read yet: ", b);
    }
    this.say(box);
  }
  lookAgain(doc, n) {
    clearTimeout(this.retry);
    this.retry = setTimeout(async () => {
      if (this.doc !== doc) return;
      try {
        const r = await this.ask(doc);
        if (this.doc !== doc) return;
        if (r.pages && r.source !== "pending") { this.show(r, doc); toast(`${doc.name} has been read`); return; }
      } catch { /* try again */ }
      if (n < 40) this.lookAgain(doc, n + 1);
    }, 3000);
  }
  mark() {
    const n = this.hits.length;
    this.hits.forEach((d, i) => d.classList.toggle("cur", i === this.cur));
    this.hitInfo.textContent = n ? `Match ${fmtNum(this.cur + 1)} of ${fmtNum(n)}` : this.noHit;
    const d = this.hits[this.cur];
    this.stage.scrollTop = d ? d.offsetTop - (this.stage.clientHeight - d.offsetHeight) / 2 : 0;
    this.stage.scrollLeft = 0;
  }
  step(d) {
    if (!this.hits.length) return;
    this.cur = (this.cur + d + this.hits.length) % this.hits.length;
    this.mark();
  }
}

// ---------- segmented source toggles ----------
function segValue(id) { return $(id).querySelector("button.on")?.dataset.src || "local"; }
function initSeg(id, onChange) {
  for (const b of $(id).querySelectorAll("button")) {
    b.type = "button";
    b.onclick = () => {
      if (b.disabled) return;
      for (const x of $(id).querySelectorAll("button")) x.classList.toggle("on", x === b);
      onChange(b.dataset.src);
    };
  }
}
/** Enable "Office server" only while connected; fall back to this PC otherwise. */
function syncSeg(id, onChange) {
  const ok = officeConnected();
  const office = $(id).querySelector('[data-src="office"]');
  office.disabled = !ok;
  office.title = ok ? "Search the office server's index" : "Only when the office server is connected";
  if (!ok && segValue(id) === "office") {
    for (const x of $(id).querySelectorAll("button")) x.classList.toggle("on", x.dataset.src === "local");
    onChange("local");
  }
}
