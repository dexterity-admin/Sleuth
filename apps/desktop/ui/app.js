// Sleuth window: worker connection, status bar, shared-cache enforcement,
// settings, updates and start-up. Loaded last (after core.js, search.js, views.js).
"use strict";

// ---------- worker connection ----------
async function waitForWorker() {
  for (let i = 0; i < 60; i++) {
    try {
      const r = await call({ req: "hello", protocol: PROTOCOL });
      if (r.protocol !== undefined && r.protocol !== PROTOCOL) {
        showBanner(`The background reader (version ${r.version || "?"}) does not match this window. Close Sleuth and open it again; if this stays, reinstall Sleuth.`, null);
      }
      return true;
    } catch { await new Promise((r) => setTimeout(r, 500)); }
  }
  showBanner("The Sleuth reader did not start. Close Sleuth and open it again.", null);
  return false;
}
listen("worker-down", () => { $("sbReader").textContent = "Reader restarting…"; });
listen("worker-up", () => { loadStatus(); });

// ---------- status ----------
async function loadStatus() {
  try {
    const r = await call({ req: "status" });
    const s = r.status;
    let t = s.reader_state === "reading" && s.current_file ? `Reading ${baseName(s.current_file)}` : cap(s.reader_state);
    t += ` · ${s.pdfs_read.toLocaleString()} PDFs read · ${s.pdfs_waiting.toLocaleString()} waiting`;
    if (s.pages_per_hour) t += ` · ${s.pages_per_hour} pages/h`;
    $("sbReader").textContent = t;
    state.lastStatus = r;
    setServer(s.server);
    enforceCache(s.cache);
  } catch { /* worker restarting */ }
}
const SERVER_BADGE = {
  connected: ["ok", "Office server connected"],
  syncing: ["run", "Office server: first reading pass"],
  offline: ["off", "Office server offline"],
  degraded: ["warn", "Office server refusing this PC"],
};
function setServer(srv) {
  const before = officeConnected();
  const beforeProjects = JSON.stringify(state.server?.projects || []);
  state.server = srv || { state: "not_configured" };
  const b = $("sbServer");
  const badge = SERVER_BADGE[state.server.state];
  b.hidden = !badge;
  if (badge) {
    b.className = "srvbadge " + badge[0];
    b.textContent = badge[1];
    b.title = (state.server.message || badge[1]) + (state.server.last_ok ? `\nLast contact ${fmtDateTime(state.server.last_ok)}` : "");
  }
  if (before !== officeConnected() || beforeProjects !== JSON.stringify(state.server.projects || [])) {
    renderProjects();
    syncSeg("tagSource", reloadTags);
    syncSeg("revSource", loadRevisions);
  }
}
$("sbServer").onclick = () => openSettings(false, "serverSection");

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
  renderProjects();
}
async function saveSettings(s) {
  await call({ req: "set_settings", settings: s });
  state.settings = s;
  state.updateChannel = s.update_channel;
  renderScopes();
  renderProjects();
  try { await invoke("set_autostart", { enabled: !!s.keep_indexing_when_closed }); } catch { /* registry blocked */ }
}
$("settingsBtn").onclick = () => openSettings(false);

let draftProjects = [];
async function openSettings(focusCache, focusSection) {
  await loadSettings();
  const s = state.settings;
  $("s-quality").value = s.ocr_quality === "high" ? "high" : "auto"; $("s-dpi").value = String(s.ocr_dpi);
  $("s-backend").value = s.ocr_backend || "auto";
  // DirectML is an IT override; offer it only when it is already chosen.
  $("s-backend").querySelector('[value="directml"]').hidden = s.ocr_backend !== "directml";
  $("s-titleblock").checked = s.ocr_missing_title_block !== false;
  $("s-busy").value = s.busy_cpu_percent; $("s-keep").checked = s.keep_indexing_when_closed;
  $("s-cache").value = s.shared_cache_dir || ""; $("s-per").value = s.listings_per_server;
  $("s-timeout").value = s.listing_timeout_secs; $("s-rescan").value = s.name_rescan_minutes;
  $("s-excluded").value = (s.excluded || []).join("\n"); $("s-channel").value = s.update_channel;
  $("s-revorder").value = s.revision_order || "letters_then_numbers";
  $("s-srvUrl").value = s.server?.url || ""; $("s-srvKey").value = ""; $("s-srvCa").value = s.server?.ca_file || "";
  $("s-srvKey").placeholder = s.server?.token_protected ? "Saved. Leave blank to keep it" : "Access key from IT";
  showServerState(state.server, false);
  $("s-tagRules").value = s.tag_rules ? JSON.stringify(s.tag_rules, null, 2) : "";
  $("s-fieldRules").value = s.field_rules ? JSON.stringify(s.field_rules, null, 2) : "";
  draftProjects = (s.projects || []).map((p) => ({ name: p.name, roots: [...(p.roots || [])] }));
  renderProjectEditor();
  $("s-version").textContent = "Version " + (await invoke("app_version"));
  $("s-error").hidden = true; $("s-updateMsg").textContent = "";
  const st = state.lastStatus?.status;
  $("s-backendNote").textContent = st?.ocr_backend ? `Currently running on: ${st.ocr_backend}.` : "";
  call({ req: "monitor" }).then((r) => { if (r.monitor?.backend_note) $("s-backendNote").textContent = `Currently running on: ${st?.ocr_backend || r.monitor.status?.ocr_backend || "?"}. ${r.monitor.backend_note}`; }).catch(() => {});
  const c = st?.cache;
  if (c) showCacheCheck(c); else $("s-cacheState").textContent = "";
  $("settingsDlg").showModal();
  checkRollback();
  if (focusCache || cacheNeedsUser(c)) highlightCache();
  else if (focusSection) $(focusSection).scrollIntoView({ block: "start" });
}
$("s-cachePick").onclick = async () => { const p = await invoke("pick_folder"); if (p) { $("s-cache").value = p; testCache().catch(() => {}); } };

// Projects editor (kept as a draft until Save).
function renderProjectEditor() {
  const box = $("s-projects");
  box.replaceChildren();
  if (!draftProjects.length) box.append(el("div", "hint", "No projects yet."));
  draftProjects.forEach((p, i) => {
    const card = el("div", "proj");
    const top = el("div", "row-inline");
    const name = el("input"); name.type = "text"; name.value = p.name; name.placeholder = "Project name, e.g. P12513"; name.setAttribute("aria-label", "Project name");
    name.oninput = () => { p.name = name.value; };
    const add = el("button", "small", "Add folder"); add.type = "button";
    add.onclick = async () => {
      const f = await invoke("pick_folder");
      if (f && !p.roots.some((r) => r.toLowerCase() === f.toLowerCase())) { p.roots.push(f); renderProjectEditor(); }
    };
    const del = el("button", "small", "Remove project"); del.type = "button";
    del.onclick = () => { draftProjects.splice(i, 1); renderProjectEditor(); };
    top.append(name, add, del);
    card.append(top);
    if (!p.roots.length) card.append(el("div", "hint", "No folders yet."));
    for (const r of p.roots) {
      const row = el("div", "scope");
      const t = el("span", null, r); t.title = r;
      const x = el("button", "icon", "×"); x.type = "button"; x.title = "Remove this folder"; x.setAttribute("aria-label", "Remove " + r);
      x.onclick = () => { p.roots = p.roots.filter((v) => v !== r); renderProjectEditor(); };
      row.append(t, x); card.append(row);
    }
    box.append(card);
  });
}
$("s-addProject").onclick = () => { draftProjects.push({ name: "", roots: [] }); renderProjectEditor(); $("s-projects").querySelector(".proj:last-child input")?.focus(); };

// Office server: these buttons act at once (the key is encrypted by the reader).
function showServerState(srv, tested) {
  const p = $("s-srvState");
  if (!srv) { p.textContent = ""; return; }
  const ok = srv.state === "connected" || srv.state === "syncing";
  p.className = "hint " + (ok ? "ok-text" : srv.state === "not_configured" ? "" : "error");
  const label = { not_configured: "Not set up: Sleuth works on its own.", connected: "Connected.", syncing: "Connected. The server is still reading its first pass.", offline: "The server does not answer.", degraded: "The server answers but refuses this PC." }[srv.state] || srv.state;
  p.textContent = (tested ? "Test: " : "Now: ") + label + (srv.version && srv.state !== "not_configured" ? ` Server version ${srv.version}.` : "") + (srv.message ? ` ${srv.message}` : "");
}
function serverFields() { return { url: $("s-srvUrl").value.trim(), token: $("s-srvKey").value, ca_file: $("s-srvCa").value.trim() }; }
$("s-srvCaPick").onclick = async () => {
  const p = await invoke("pick_file", { title: "Office server certificate", name: "Certificate", extensions: ["pem", "crt", "cer"] });
  if (p) $("s-srvCa").value = p;
};
$("s-srvTest").onclick = async () => {
  const f = serverFields();
  if (!f.url) { $("s-srvState").className = "hint error"; $("s-srvState").textContent = "Type the server address first."; return; }
  $("s-srvState").className = "hint"; $("s-srvState").textContent = "Testing…";
  try { const r = await call({ req: "check_server", ...f }); showServerState(r.server, true); }
  catch (e) { $("s-srvState").className = "hint error"; $("s-srvState").textContent = String(e); }
};
async function refreshServerSettings() {
  // The reader saved the server part itself; keep it so Save does not overwrite it.
  try { const r = await call({ req: "get_settings" }); state.settings.server = r.settings.server; } catch { /* keep */ }
  $("s-srvKey").value = "";
  $("s-srvKey").placeholder = state.settings.server?.token_protected ? "Saved. Leave blank to keep it" : "Access key from IT";
  await loadStatus();
  showServerState(state.server, false);
}
$("s-srvSave").onclick = async () => {
  const f = serverFields();
  if (!f.url) { $("s-srvState").className = "hint error"; $("s-srvState").textContent = "Type the server address first, or use Disconnect."; return; }
  $("s-srvState").className = "hint"; $("s-srvState").textContent = "Connecting…";
  try { await call({ req: "set_server", ...f }); await refreshServerSettings(); toast("Office server saved"); }
  catch (e) { $("s-srvState").className = "hint error"; $("s-srvState").textContent = String(e); }
};
$("s-srvOff").onclick = async () => {
  if (!(await confirmBox("Disconnect from the office server?", ["Searches will cover this PC's index only. The saved access key is removed."], "Disconnect"))) return;
  try {
    await call({ req: "set_server", url: "", token: "", ca_file: "" });
    $("s-srvUrl").value = ""; $("s-srvCa").value = "";
    await refreshServerSettings();
    toast("Disconnected from the office server");
  } catch (e) { $("s-srvState").className = "hint error"; $("s-srvState").textContent = String(e); }
};

// Advanced rules. Copy of the built-in rules in crates/sleuth-core/src/settings.rs,
// used only to give an editable starting point.
const BUILTIN_TAG_RULES = [
  { category: "line", pattern: String.raw`\b\d{1,2}(?:"|''|IN)?-[A-Z]{1,4}-\d{3,6}(?:-[A-Z0-9]{1,6})+\b`, enabled: true },
  { category: "drawing", pattern: String.raw`\b(?:ISO|PID|P&ID|GA|DWG|DOC|STR|LAY)-[A-Z0-9]+(?:-[A-Z0-9]+)*\b`, enabled: true },
  { category: "instrument", pattern: String.raw`\b(?:[FPTLAHSXZ][A-Z]{1,3})[- ]?\d{3,5}[A-Z]?\b`, enabled: true },
  { category: "valve", pattern: String.raw`\b(?:V|MOV|XV|SDV|BDV)-\d{3,5}[A-Z]?\b`, enabled: true },
  { category: "equipment", pattern: String.raw`\b\d{0,3}[A-Z]{1,2}-\d{3,4}[A-Z]?\b`, enabled: true },
  { category: "support", pattern: String.raw`\b(?:PS|SP|SUP)-?[A-Z]{0,2}-?\d{2,6}\b`, enabled: true },
  { category: "weld", pattern: String.raw`\b(?:FW|SW|BW|W)-\d{1,4}\b`, enabled: true },
  { category: "spec", pattern: String.raw`\b(?:SPEC|PIPING CLASS|CLASS)[ .:]+([A-Z0-9]{2,8})\b`, enabled: true },
];
const BUILTIN_FIELD_RULES = [
  { field: "drawing_number", labels: ["DRAWING NUMBER", "DRAWING NO", "DRG NO", "DWG NO", "DOCUMENT NO", "DOC NO"], value: String.raw`[A-Z0-9][A-Z0-9\-_/.]{3,40}` },
  { field: "revision", labels: ["REVISION", "REV"], value: String.raw`[A-Z0-9]{1,3}\b` },
  { field: "title", labels: ["DRAWING TITLE", "DRG TITLE", "DWG TITLE", "TITLE"], value: ".{3,100}" },
  { field: "project", labels: ["PROJECT NUMBER", "PROJECT NO", "JOB NO", "PROJECT"], value: ".{2,60}" },
  { field: "sheet", labels: ["SHEET"], value: String.raw`\d+(?:\s*OF\s*\d+)?` },
  { field: "discipline", labels: ["DISCIPLINE"], value: "[A-Z][A-Z &/-]{2,30}" },
  { field: "status", labels: ["STATUS"], value: "[A-Z][A-Z0-9 -]{2,30}" },
];
$("s-tagRulesCopy").onclick = () => { if (!$("s-tagRules").value.trim()) $("s-tagRules").value = JSON.stringify(BUILTIN_TAG_RULES, null, 2); };
$("s-fieldRulesCopy").onclick = () => { if (!$("s-fieldRules").value.trim()) $("s-fieldRules").value = JSON.stringify(BUILTIN_FIELD_RULES, null, 2); };
$("s-tagRulesReset").onclick = () => { $("s-tagRules").value = ""; };
$("s-fieldRulesReset").onclick = () => { $("s-fieldRules").value = ""; };
function parseRules(id, what, check) {
  const t = $(id).value.trim();
  if (!t) return null;
  let v;
  try { v = JSON.parse(t); } catch (e) { throw new Error(`${what}: not valid JSON (${e.message}). Fix it, or empty the box to use the built-in rules.`); }
  if (!Array.isArray(v)) throw new Error(`${what}: must be a list in [ ].`);
  v.forEach((r, i) => { const m = check(r); if (m) throw new Error(`${what}, rule ${i + 1}: ${m}`); });
  return v;
}
const isStr = (x) => typeof x === "string" && x.length > 0;
function checkTagRule(r) {
  if (!r || typeof r !== "object" || Array.isArray(r)) return "must be an object { }";
  if (!isStr(r.category)) return "needs a \"category\"";
  if (!isStr(r.pattern)) return "needs a \"pattern\"";
  if (r.enabled !== undefined && typeof r.enabled !== "boolean") return "\"enabled\" must be true or false";
  return null;
}
function checkFieldRule(r) {
  if (!r || typeof r !== "object" || Array.isArray(r)) return "must be an object { }";
  if (!isStr(r.field)) return "needs a \"field\"";
  if (!Array.isArray(r.labels) || !r.labels.length || !r.labels.every(isStr)) return "needs \"labels\": a list of words printed before the value";
  if (!isStr(r.value)) return "needs a \"value\" pattern";
  return null;
}

$("s-save").onclick = async () => {
  const num = (id, lo, hi) => { const v = Number($(id).value); if (!Number.isFinite(v) || v < lo || v > hi) throw new Error(`Enter a number from ${lo} to ${hi}`); return Math.round(v); };
  $("s-error").hidden = true;
  try {
    const tagRules = parseRules("s-tagRules", "Tag rules", checkTagRule);
    const fieldRules = parseRules("s-fieldRules", "Title-block rules", checkFieldRule);
    const projects = draftProjects.map((p) => ({ name: p.name.trim(), roots: p.roots })).filter((p) => p.name || p.roots.length);
    const names = new Set();
    for (const p of projects) {
      if (!p.name) throw new Error("Every project needs a name.");
      if (names.has(p.name.toLowerCase())) throw new Error(`Two projects are called "${p.name}".`);
      names.add(p.name.toLowerCase());
    }
    const s = {
      ...state.settings,
      ocr_quality: $("s-quality").value,
      ocr_dpi: Number($("s-dpi").value),
      ocr_backend: $("s-backend").value,
      ocr_missing_title_block: $("s-titleblock").checked,
      busy_cpu_percent: num("s-busy", 10, 100),
      keep_indexing_when_closed: $("s-keep").checked,
      shared_cache_dir: $("s-cache").value.trim() || null,
      listings_per_server: num("s-per", 1, 16),
      listing_timeout_secs: num("s-timeout", 5, 120),
      name_rescan_minutes: num("s-rescan", 15, 10080),
      excluded: $("s-excluded").value.split(/\r?\n/).map((x) => x.trim()).filter(Boolean),
      update_channel: $("s-channel").value,
      revision_order: $("s-revorder").value,
      projects,
      tag_rules: tagRules,
      field_rules: fieldRules,
    };
    const c = await testCache();
    if (c.state === "not_set" || c.state === "no_access") {
      highlightCache();
      throw new Error("Choose a shared folder that works before saving. Every PC needs the shared reading cache.");
    }
    await saveSettings(s);
    $("settingsDlg").close();
    loadStatus();
    toast("Settings saved" + (tagRules || fieldRules ? ". Rule problems, if any, are listed under Monitor → Rule errors." : ""));
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
  checkRollback();
};
$("s-channel").onchange = () => checkRollback();

// ---------- updates ----------
function showBanner(text, actionLabel, action) {
  $("bannerText").textContent = text;
  $("bannerAction").hidden = !actionLabel;
  $("bannerAction").disabled = false;
  if (actionLabel) { $("bannerAction").textContent = actionLabel; $("bannerAction").onclick = action; }
  $("banner").hidden = false;
}
$("bannerClose").onclick = () => { $("banner").hidden = true; };
async function runInstall(command, channel) {
  $("bannerAction").disabled = true;
  const un = await listen("update-progress", (e) => {
    const { done, total } = e.payload;
    $("bannerText").textContent = total ? `Downloading… ${Math.round((done / total) * 100)}%` : "Downloading…";
  });
  try { await invoke(command, { channel }); }
  catch (e) { $("bannerText").textContent = String(e); $("bannerAction").disabled = false; }
  finally { un(); }
}
function offerUpdate(u, channel) {
  showBanner(`Sleuth ${u.version} is available. Your settings and index are kept.`, "Restart and update", () => runInstall("install_update", channel));
}
async function autoCheckUpdate() {
  try { const u = await invoke("check_update", { channel: state.updateChannel }); if (u) offerUpdate(u, state.updateChannel); }
  catch { /* offline: try again later */ }
}

// ---------- roll back to the previous release ----------
async function checkRollback() {
  const row = $("s-rollbackRow");
  const channel = $("s-channel").value;
  try {
    const t = await invoke("check_rollback", { channel });
    if (!t || channel !== $("s-channel").value) { row.hidden = true; return; }
    row.hidden = false;
    $("s-rollback").textContent = `Roll back to version ${t.version}`;
    $("s-rollbackMsg").textContent = "";
    $("s-rollback").onclick = async () => {
      const ok = await confirmBox(`Roll back to Sleuth ${t.version}?`, [
        "Sleuth closes, installs the earlier version and starts again. The installer is checked against its published checksum first, the same as an update.",
        "Your settings and the index are kept. An index made by 1.0 is still readable by 0.1, because changes to it only add to it, but 0.1 will not show the 1.0 features (tags, revisions, corrections, office server).",
      ], "Roll back and restart");
      if (!ok) return;
      $("settingsDlg").close();
      showBanner(`Rolling back to Sleuth ${t.version}…`, "Retry", () => runInstall("install_rollback", channel));
      runInstall("install_rollback", channel);
    };
  } catch { row.hidden = true; }
}

// ---------- keyboard ----------
document.addEventListener("keydown", (e) => {
  if (e.ctrlKey && e.key.toLowerCase() === "f") {
    e.preventDefault();
    if (state.view_ !== "search") showView("search");
    $("q").focus(); $("q").select();
  }
  if (e.key === "Escape" && !document.querySelector("dialog[open]")) {
    $("ctx").hidden = true; $("exportMenu").hidden = true; $("syntaxPop").hidden = true;
  }
});

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
