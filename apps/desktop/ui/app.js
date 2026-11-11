// Sleuth window: worker connection, status bar, stage controls, the office
// index folder, settings, updates and start-up. Loaded last (after core.js,
// search.js, views.js).
"use strict";

// ---------- worker connection ----------
/** Ask the reader which protocol it speaks; an older one gets none of the 1.1 controls. */
async function hello() {
  const r = await call({ req: "hello", protocol: PROTOCOL });
  state.proto = Number(r.protocol) || 0;
  $("typeFilters").hidden = !newReader();
  return r;
}
async function waitForWorker() {
  for (let i = 0; i < 60; i++) {
    try {
      const r = await hello();
      if (r.protocol !== undefined && r.protocol < PROTOCOL) {
        showBanner(`The background reader (version ${r.version || "?"}) is older than this window, so the newer controls are hidden. Close Sleuth and open it again; if this stays, reinstall Sleuth.`, null);
      } else if (r.protocol !== undefined && r.protocol !== PROTOCOL) {
        showBanner(`The background reader (version ${r.version || "?"}) does not match this window. Close Sleuth and open it again; if this stays, reinstall Sleuth.`, null);
      }
      return true;
    } catch { await new Promise((r) => setTimeout(r, 500)); }
  }
  showBanner("The Sleuth reader did not start. Close Sleuth and open it again.", null);
  return false;
}
listen("worker-down", () => {
  closeStageMenu();
  paintStage("stage1", { cls: "warn", text: "The background reader is restarting…", title: "", bar: 0 });
  paintStage("stage2", { cls: "warn", text: "", title: "", bar: 0 });
});
listen("worker-up", () => { hello().catch(() => {}).then(() => loadStatus()); });

// ---------- status ----------
// Polled every 2 s while files are being listed or read, every 10 s otherwise,
// so every view shows live numbers without pressing Refresh.
const POLL_BUSY_MS = 2000, POLL_IDLE_MS = 10000;
let pollTimer = 0, pollFastUntil = 0, statusBusy = false;
function schedulePoll() {
  clearTimeout(pollTimer);
  const fast = statusBusy || Date.now() < pollFastUntil;
  pollTimer = setTimeout(loadStatus, fast ? POLL_BUSY_MS : POLL_IDLE_MS);
}
/** Poll quickly for a while, e.g. after a reading folder was added. */
function pollFast(ms) { pollFastUntil = Date.now() + (ms || 120000); schedulePoll(); }
let statusInFlight = null;
function loadStatus() {
  if (!statusInFlight) statusInFlight = fetchStatus().finally(() => { statusInFlight = null; schedulePoll(); });
  return statusInFlight;
}
async function fetchStatus() {
  try {
    const r = await call({ req: "status" });
    if (stagePatch) Object.assign(r.status, stagePatch); // a click not confirmed yet
    useStatus(r);
  } catch { /* worker restarting */ }
}
function useStatus(r) {
  const s = r.status;
  state.lastStatus = r;
  syncSettings(s);
  const p = progressModel(s);
  statusBusy = p.walking || p.reading || /^loading/i.test(s.reader_state || "");
  renderStages(p);
  setServer(s.server);
  enforceCache(s);
  onStatus(p);
}
/** Pausing and the speed are saved by the reader itself: keep our copy of the settings in step, so a later Save does not undo them. */
function syncSettings(s) {
  const st = state.settings;
  if (!st) return;
  if (typeof s.walk_paused === "boolean") st.pause_walk = s.walk_paused;
  if (typeof s.read_paused === "boolean") st.pause_reading = s.read_paused;
  if (SPEED_LABEL[s.speed] && !(Number(s.speed_until) > 0)) st.read_speed = s.speed;
}
function paintStage(id, st) {
  const b = $(id);
  b.className = "stage " + st.cls;
  b.querySelector(".st-text").textContent = st.text;
  const word = b.querySelector(".st-state");
  word.textContent = st.state || ""; word.hidden = !st.state; word.className = "st-state pill st-" + st.cls;
  b.title = (st.state ? st.state + " " : "") + st.text + "\n" + (st.title ? st.title + "\n" : "") + "Click for details";
  const fill = b.querySelector(".st-fill");
  fill.classList.toggle("moving", st.bar === null);
  fill.style.width = st.bar === null ? "" : Math.round(Math.min(1, Math.max(0, st.bar)) * 100) + "%";
}
function renderStages(p) {
  paintStage("stage1", stageFind(p));
  paintStage("stage2", stageRead(p));
  // The controls need the 1.1 reader; a paused stage keeps Resume one click away.
  const manual = newReader() && !!p.ix;
  $("stage1Ctl").hidden = $("stage2Ctl").hidden = !manual;
  $("stage1Resume").hidden = !p.walkPaused;
  $("stage2Resume").hidden = !p.readPaused;
  if (!manual) closeStageMenu();
}

// ---------- stage controls ----------
// Pause, resume and start each stage by hand on this PC (set_paused, run_now,
// set_speed). The strip's menus, Monitor and Settings all draw the same
// actions from the latest status. A click shows at once: `stagePatch` is laid
// over the status until the reader's next status confirms it.
let stagePatch = null, stagePending = 0;
async function stageAct(req, patch, said) {
  closeStageMenu(true);
  stagePatch = { ...stagePatch, ...patch };
  stagePending++;
  if (state.lastStatus) { Object.assign(state.lastStatus.status, patch); useStatus(state.lastStatus); }
  let failed = null;
  try { await call(req); } catch (e) { failed = e; }
  if (statusInFlight) await statusInFlight; // asked before the click: shown with the patch laid over it
  if (failed && stagePatch) for (const k of Object.keys(patch)) delete stagePatch[k];
  if (--stagePending === 0) stagePatch = null;
  toast(failed ? String(failed) : said);
  pollFast(30000);
  await loadStatus();
}
function setPaused(stage, paused) {
  const what = stage === "walk" ? "Finding files" : "Reading";
  return stageAct({ req: "set_paused", stage, paused }, stage === "walk" ? { walk_paused: paused } : { read_paused: paused },
    paused ? `${what} is paused on this PC until you resume it. Other PCs carry on.` : `${what} resumed on this PC.`);
}
/** Do a stage's work now: list every drive again (or one, `root`), or start reading at once. Also ends a pause. */
function runNow(stage, root, name) {
  const req = { req: "run_now", stage };
  if (root) req.root = root;
  if (stage === "walk") return stageAct(req, { walk_paused: false }, root ? `Listing ${name || root} again now.` : "Listing every drive again now. Progress shows at the bottom of the window.");
  return stageAct(req, { read_paused: false }, "Reading starts now, without waiting for the file list.");
}
function setSpeed(mode, minutes) {
  const until = mode === "full" && minutes ? nowSecs() + minutes * 60 : 0;
  // A speed without a time limit is also the saved setting: keep an open Settings form in step.
  if (!until && $("settingsDlg").open) $("s-speed").value = mode;
  return stageAct({ req: "set_speed", mode, minutes: minutes || 0 }, { speed: mode, speed_until: until },
    until ? `Full speed until ${fmtClock(until)}, then back to the speed set in Settings.`
      : mode === "full" ? "Full speed until you change it." : mode === "gentle" ? "Reading at the minimum only, until you change it." : "Reading speed is automatic.");
}
/** The manual controls of a stage as the status stands: [{key, label, hint, run, pick, cur} | {sep}]. */
function stageActions(stage, p) {
  if (stage === "walk") return [
    p.walkPaused
      ? { key: "walk-pause", label: "Resume finding files", hint: "Carry on listing the drives on this PC.", run: () => setPaused("walk", false) }
      : { key: "walk-pause", label: "Pause finding files", hint: "Stop listing drives on this PC until you resume it. Other PCs carry on.", run: () => setPaused("walk", true) },
    { key: "walk-now", label: "Find files now", hint: "List every drive again now, to pick up new, moved and deleted files.", run: () => runNow("walk") },
  ];
  const timed = p.speedUntil;
  const acts = [
    p.readPaused
      ? { key: "read-pause", label: "Resume reading", hint: "Carry on reading documents on this PC.", run: () => setPaused("read", false) }
      : { key: "read-pause", label: "Pause reading", hint: "Stop reading documents on this PC until you resume it. Other PCs carry on.", run: () => setPaused("read", true) },
    { key: "read-now", label: "Read now", hint: "Start reading at once, without waiting for the file list.", run: () => runNow("read") },
    { sep: true },
    { key: "speed-hour", pick: true, cur: !!timed, label: timed ? `Full speed until ${fmtClock(timed)}` : "Full speed for 1 hour", hint: "Every core for an hour, even while the PC is in use; then back to the speed set in Settings.", run: () => setSpeed("full", 60) },
  ];
  if (p.speed === "full" && !timed) acts.push({ key: "speed-full", pick: true, cur: true, label: "Full speed, until changed", hint: "Every core, even while the PC is in use.", run: () => setSpeed("full", 0) });
  acts.push(
    { key: "speed-gentle", pick: true, cur: p.speed === "gentle", label: "Gentle (minimum only)", hint: "The minimum only, whatever the PC is doing.", run: () => setSpeed("gentle", 0) },
    { key: "speed-auto", pick: true, cur: p.speed === "auto", label: "Automatic speed", hint: "As fast as the PC allows without getting in your way.", run: () => setSpeed("auto", 0) },
  );
  return acts;
}
/** The same actions as plain buttons, each with its line of explanation (Monitor); `compact`: buttons only, explanation as tooltip (Settings). */
function stageButtons(stage, p, compact) {
  const box = el("div", compact ? "idx-acts" : "stage-acts");
  for (const a of stageActions(stage, p)) {
    if (a.sep || (compact && a.key === "walk-now")) continue; // Settings has "Find files now" beside Save
    const b = el("button", "small" + (a.cur ? " cur" : ""), a.label); b.type = "button";
    b.dataset.key = a.key; b.title = a.hint;
    if (a.pick) b.setAttribute("aria-pressed", String(!!a.cur));
    b.onclick = a.run;
    if (compact) { box.append(b); continue; }
    const row = el("div", "stage-act");
    row.append(b, el("span", null, a.hint));
    box.append(row);
  }
  return box;
}
// The menu of a stage in the strip.
let stageMenuFor = null;
function openStageMenu(btn) {
  const m = $("stageMenu");
  m.replaceChildren();
  for (const a of stageActions(btn.dataset.stage, progressModel(state.lastStatus?.status))) {
    if (a.sep) { m.append(el("div", "sep")); continue; }
    const b = el("button", (a.pick ? "pick" : "") + (a.cur ? " cur" : "")); b.type = "button";
    b.setAttribute("role", a.pick ? "menuitemradio" : "menuitem");
    if (a.pick) b.setAttribute("aria-checked", String(!!a.cur));
    b.append(el("span", "mi-label", a.label), el("span", "mi-hint", a.hint));
    b.onclick = a.run;
    m.append(b);
  }
  m.hidden = false;
  const r = btn.getBoundingClientRect();
  m.style.left = Math.max(8, Math.min(r.right - m.offsetWidth, innerWidth - m.offsetWidth - 8)) + "px";
  m.style.top = Math.max(8, r.top - m.offsetHeight - 6) + "px";
  stageMenuFor = btn;
  btn.setAttribute("aria-expanded", "true");
  m.querySelector("button")?.focus();
}
function closeStageMenu(refocus) {
  const m = $("stageMenu");
  if (m.hidden) return;
  m.hidden = true;
  stageMenuFor?.setAttribute("aria-expanded", "false");
  if (refocus) stageMenuFor?.focus();
  stageMenuFor = null;
}
for (const id of ["stage1Menu", "stage2Menu"]) {
  $(id).onclick = (e) => {
    e.stopPropagation();
    const again = stageMenuFor === $(id);
    closeStageMenu();
    if (!again) openStageMenu($(id));
  };
}
$("stageMenu").addEventListener("click", (e) => e.stopPropagation());
$("stageMenu").addEventListener("keydown", (e) => {
  const items = [...$("stageMenu").querySelectorAll("button")];
  const i = items.indexOf(document.activeElement);
  if (e.key === "ArrowDown") { e.preventDefault(); items[(i + 1) % items.length]?.focus(); }
  else if (e.key === "ArrowUp") { e.preventDefault(); items[(i - 1 + items.length) % items.length]?.focus(); }
  else if (e.key === "Home") { e.preventDefault(); items[0]?.focus(); }
  else if (e.key === "End") { e.preventDefault(); items[items.length - 1]?.focus(); }
  else if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); closeStageMenu(true); }
  else if (e.key === "Tab") closeStageMenu();
});
document.addEventListener("click", () => closeStageMenu());
$("stage1Resume").onclick = () => setPaused("walk", false);
$("stage2Resume").onclick = () => setPaused("read", false);
/** Everything else that shows progress follows the latest status. */
function onStatus(p) {
  updateWalkBanner(p);
  if (!state.query) renderEmpty();
  renderScopes();
  scopeFeedback(p);
  if (state.view_ === "monitor") renderIndexing();
  if ($("settingsDlg").open) renderSettingsIndex(p);
}
for (const id of ["stage1", "stage2"]) $(id).onclick = () => showIndexing();
$("walkBannerMore").onclick = () => showIndexing();
function showIndexing() {
  if ($("settingsDlg").open) $("settingsDlg").close();
  showView("monitor");
  $("monBody").scrollTop = 0;
}

// Search banner: results can be incomplete while the drives are still listed.
function updateWalkBanner(p) {
  p = p || progressModel(state.lastStatus?.status);
  const lines = [];
  const ix = p.ix;
  if (ix && p.walking && state.query) {
    if (p.firstWalk) lines.push(`Still finding files — results may be incomplete (${plural(ix.files_seen || p.s.files_indexed, "file")} found so far).`);
    else lines.push(`Checking the drives for new files (${plural(ix.files_seen, "file")} checked so far). Files added since ${fmtAgo(ix.last_complete)} may not show yet.`);
  }
  if (ix && p.walkPaused && p.firstWalk && state.query) lines.push("Finding files is paused on this PC before its first listing finished, so results may be incomplete.");
  if (state.query && state.view.length && looksLikeFileName(state.query) && p.waiting > 0 && p.scopeCount) {
    lines.push(`Text inside documents is searched only in documents already read: ${fmtNum(p.read)} of ${fmtNum(p.total)} in your reading folders so far.`);
  }
  $("walkBannerText").textContent = lines.join(" ");
  $("walkBanner").hidden = !lines.length || state.view_ !== "search";
}
function looksLikeFileName(q) {
  q = String(q || "").trim();
  if (!q || /\s/.test(q) || /:/.test(q)) return false;
  if (/\.[a-z0-9]{2,4}$/i.test(q)) return true;
  return /[a-z]/i.test(q) && /\d/.test(q) && /[-_]/.test(q);
}

// Feedback after a reading folder is added: show the queue once it is counted.
let scopeWatch = null; // {folder, since, told}
function watchScope(folder) { scopeWatch = { folder, since: Date.now(), told: "" }; pollFast(); }
function scopeFeedback(p) {
  if (!scopeWatch) return;
  const sc = (p.scopes || []).find((x) => String(x.folder).toLowerCase() === scopeWatch.folder.toLowerCase());
  let msg = "";
  if (!p.scopes) msg = `${scopeWatch.folder} added. Its documents will be read in the background.`;
  else if (!sc) msg = `${scopeWatch.folder} added. Counting its documents…`;
  else if (sc.note && sc.note !== "ok" && sc.note !== "not indexed yet") msg = `${scopeWatch.folder}: ${sc.note}. ${SCOPE_NOTE[sc.note] || ""}`;
  else if (sc.pdfs > 0) msg = `${sc.folder} — ${plural(sc.waiting, "document")} queued for reading` + (sc.read ? `, ${fmtNum(sc.read)} already read` : "") + ".";
  else if (sc.note === "not indexed yet" || p.walking) msg = `${scopeWatch.folder} added. Its documents are queued as soon as stage 1 lists it.`;
  else msg = `${scopeWatch.folder} added. No documents found in it.`;
  if (msg !== scopeWatch.told) { toast(msg); scopeWatch.told = msg; }
  const settled = sc && (sc.pdfs > 0 || (sc.note && !["ok", "not indexed yet"].includes(sc.note)));
  if (settled || !p.scopes || Date.now() - scopeWatch.since > 10 * 60000) scopeWatch = null;
}

// Settings: the same two lines, with the same manual buttons, above "Find files now".
holdWhilePressed($("s-indexState"));
function renderSettingsIndex(p) {
  p = p || progressModel(state.lastStatus?.status);
  const box = $("s-indexState");
  const manual = newReader() && !!p.ix;
  redrawKeeping(box, () => {
    box.replaceChildren();
    for (const [stage, name, st] of [["walk", "Finding files", stageFind(p)], ["read", "Reading documents", stageRead(p)]]) {
      const row = el("div", "idx-line " + st.cls);
      row.append(el("span", "st-dot"), el("b", null, name + ": "), stateWord(st), el("span", null, st.text));
      box.append(row);
      if (manual) box.append(stageButtons(stage, p, true));
    }
    if (manual) box.append(el("div", "idx-note", "These buttons act at once, on this PC only. They do not wait for Save."));
  });
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

// ---------- office index folder: required on every PC ----------
// Not set up -> a message that cannot be dismissed. Ignored for a minute ->
// Settings opens with the folder section highlighted. Closing Settings without
// fixing it starts the same cycle again. While the share cannot be reached the
// notice stays (1.1: text inside documents cannot be searched until it is back).
const CACHE_NAG_MS = 60 * 1000;
let cacheNagSince = 0;
function cacheNeedsUser(c) { return c && (c.state === "not_set" || c.state === "no_access"); }
/** The office index folder needs setting up on this PC (1.1: status.store; before: status.cache). */
function storeNeedsUser(s) {
  const st = s?.store?.state;
  return st ? st === "not set" || st === "no access" : !!cacheNeedsUser(s?.cache);
}
function enforceCache(s) {
  const st = s.store && s.store.state ? s.store : null;
  if (!st) { enforceOldCache(s.cache); return; }
  const b = $("cacheBanner");
  const say = (text, action) => {
    $("cacheBannerText").textContent = text;
    $("cacheBannerAction").hidden = !action; $("cacheBannerClose").hidden = true;
    b.hidden = false;
  };
  if (storeNeedsUser(s)) {
    say(`The office index folder isn't working on this PC${st.message ? ": " + st.message : ""}. Set it up so this PC searches the office index and documents already read on other PCs aren't read again here.`, true);
    if (!cacheNagSince) cacheNagSince = Date.now();
    if (Date.now() - cacheNagSince > CACHE_NAG_MS && !$("settingsDlg").open) openSettings(true);
    return;
  }
  cacheNagSince = 0;
  if (st.state === "offline") {
    say(`The office index folder (${st.path}) is not reachable. Searching text inside documents resumes when it is back` + (st.unpublished > 0 ? `; this PC keeps ${plural(st.unpublished, "new reading")} until then.` : "."), false);
  } else if (st.state === "disabled") {
    say(`The office index is switched off on this PC${st.message ? ": " + st.message : ""}.`, false);
  } else b.hidden = true;
}
// An older background reader: its shared folder is only a store of finished readings.
function enforceOldCache(c) {
  if (!c) return;
  const b = $("cacheBanner");
  if (cacheNeedsUser(c)) {
    $("cacheBannerText").textContent = `The office index folder isn't working on this PC: ${c.message}. Set it up so drawings already read on other PCs aren't read again here.`;
    $("cacheBannerAction").hidden = false; $("cacheBannerClose").hidden = true;
    b.hidden = false;
    if (!cacheNagSince) cacheNagSince = Date.now();
    if (Date.now() - cacheNagSince > CACHE_NAG_MS && !$("settingsDlg").open) openSettings(true);
  } else if (c.state === "offline") {
    cacheNagSince = 0;
    if (!b.dataset.dismissed) {
      $("cacheBannerText").textContent = `The office index folder (${c.path}) can't be reached right now. Sleuth keeps reading on this PC and will use it again when it's back.`;
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
$("settingsDlg").addEventListener("close", () => { if (storeNeedsUser(state.lastStatus?.status)) cacheNagSince = Date.now(); });

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
  // 1.1 settings; an older reader's settings do not have them, and the controls stay hidden.
  $("s-speedRow").hidden = !SPEED_LABEL[s.read_speed];
  $("s-speed").value = SPEED_LABEL[s.read_speed] ? s.read_speed : "auto";
  $("kindsSection").hidden = !Array.isArray(s.kinds_not_read);
  $("s-kinds").replaceChildren(...Object.keys(FILE_KIND).map((k) => {
    const lab = el("label", "check");
    const cb = el("input"); cb.type = "checkbox"; cb.dataset.kind = k; cb.checked = !(s.kinds_not_read || []).includes(k);
    lab.append(cb, " " + KIND_LONG[k]);
    return lab;
  }));
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
  $("s-error").hidden = true;
  $("s-updateMsg").textContent = upd.ready ? `Version ${upd.ready.version} is ready to install` : (upd.lastCheck ? `Up to date (checked ${new Date(upd.lastCheck).toLocaleTimeString()}). Sleuth checks every hour by itself.` : "Sleuth checks for updates by itself every hour.");
  const st = state.lastStatus?.status;
  $("s-backendNote").textContent = st?.ocr_backend ? `Currently running on: ${st.ocr_backend}.` : "";
  call({ req: "monitor" }).then((r) => { if (r.monitor?.backend_note) $("s-backendNote").textContent = `Currently running on: ${st?.ocr_backend || r.monitor.status?.ocr_backend || "?"}. ${r.monitor.backend_note}`; }).catch(() => {});
  const c = st?.cache;
  if (c) showCacheCheck(c); else $("s-cacheState").textContent = "";
  renderSettingsIndex();
  $("settingsDlg").showModal();
  checkRollback();
  if (focusCache || storeNeedsUser(st)) highlightCache();
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
    if (!$("s-speedRow").hidden) s.read_speed = $("s-speed").value;
    if (!$("kindsSection").hidden) {
      // Unticked = found by file name only. Types this window does not list stay as they were.
      const boxes = [...$("s-kinds").querySelectorAll("input")];
      const listed = boxes.map((b) => b.dataset.kind);
      s.kinds_not_read = [...(state.settings.kinds_not_read || []).filter((k) => !listed.includes(k)), ...boxes.filter((b) => !b.checked).map((b) => b.dataset.kind)];
    }
    const c = await testCache();
    if (c.state === "not_set" || c.state === "no_access") {
      highlightCache();
      throw new Error("Choose a shared folder that works before saving. Every PC needs the office index folder.");
    }
    await saveSettings(s);
    $("settingsDlg").close();
    loadStatus();
    toast("Settings saved" + (tagRules || fieldRules ? ". Rule problems, if any, are listed under Monitor → Rule errors." : ""));
  } catch (e) { $("s-error").textContent = e.message || String(e); $("s-error").hidden = false; }
};
$("s-rescanNow").onclick = async () => {
  if (newReader()) { runNow("walk"); return; }
  await call({ req: "rescan_now" });
  toast("Checking the drives for new files in the background. Progress shows at the bottom of the window.");
  pollFast(60000); loadStatus();
};
$("s-checkUpdate").onclick = async () => {
  $("s-updateMsg").textContent = "Checking…";
  try {
    const u = await prepareUpdate($("s-channel").value, false);
    if (u) { $("settingsDlg").close(); askUpdate(u, $("s-channel").value, true); }
    else $("s-updateMsg").textContent = "You have the latest version";
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
  catch (e) { $("bannerText").textContent = `Not installed: ${e}. Sleuth keeps running; try again.`; $("bannerAction").disabled = false; }
  finally { un(); }
}
// Updates are found by themselves: 10 s after start, then every hour and
// whenever the window comes back after an hour away. A newer release is
// downloaded and checked in the background first, then the user is asked once
// per version and start (a banner and a status-bar button stay until it is
// installed).
const UPDATE_EVERY = 3600 * 1000;
const upd = { ready: null, busy: null, lastCheck: 0, asked: "" };
function noteLines(notes) {
  // The first paragraph of the release notes, unwrapped and without Markdown.
  const para = String(notes || "").trim().split(/\r?\n\s*\r?\n/)[0] || "";
  const text = para.split(/\r?\n/).map((l) => l.trim()).join(" ").replace(/\*\*|`/g, "");
  return text ? [text.length > 400 ? text.slice(0, 400).replace(/\s+\S*$/, "") + "…" : text] : [];
}
function startUpdate(channel) {
  lsSet("sleuth.updatingTo", upd.ready?.version || "");
  $("banner").hidden = false;
  showBanner(`Installing Sleuth ${upd.ready?.version || ""}…`, "Restart and update", () => runInstall("install_update", channel));
  runInstall("install_update", channel);
}
function offerUpdate(u, channel) {
  upd.ready = u;
  showBanner(`Sleuth ${u.version} is ready to install. Your settings and index are kept.`, "Restart and update", () => startUpdate(channel));
  const b = $("sbUpdate");
  b.hidden = false;
  b.textContent = `Update ${u.version} ready`;
  b.title = "Restart Sleuth and install the update. Your settings and index are kept.";
  b.onclick = () => askUpdate(u, channel, true);
  if ($("settingsDlg").open) $("s-updateMsg").textContent = `Version ${u.version} is ready to install`;
}
async function askUpdate(u, channel, force) {
  if (!force && (upd.asked === u.version || document.querySelector("dialog[open]"))) return;
  upd.asked = u.version;
  const yes = await confirmBox(`Sleuth ${u.version} is ready to install`, [
    `You have version ${await invoke("app_version")}. The update is downloaded and checked. Sleuth closes for a few seconds, installs it and opens again. Your settings, reading folders and index are kept.`,
    ...noteLines(u.notes),
  ], "Restart and update", "Later");
  if (yes) startUpdate(channel);
}
/** Find, download and offer a newer release. Rejects when offline; callers decide whether to say so. */
function prepareUpdate(channel, ask) {
  if (upd.busy) return upd.busy;
  upd.lastCheck = Date.now();
  upd.busy = (async () => {
    try {
      const u = await invoke("check_update", { channel });
      if (!u) return null;
      if (!upd.ready || upd.ready.version !== u.version) {
        showBanner(`Sleuth ${u.version} is available. Downloading it in the background…`);
        await invoke("prepare_update", { channel });
      }
      offerUpdate(u, channel);
      if (ask) askUpdate(u, channel, false);
      return u;
    } catch (e) {
      if (upd.ready) offerUpdate(upd.ready, channel); else $("banner").hidden = true;
      throw e;
    } finally { upd.busy = null; }
  })();
  return upd.busy;
}
function autoCheckUpdate() { prepareUpdate(state.updateChannel, true).catch(() => { /* offline: try again later */ }); }
window.addEventListener("focus", () => { if (Date.now() - upd.lastCheck > UPDATE_EVERY) autoCheckUpdate(); });
async function sayUpdated() {
  const to = lsGet("sleuth.updatingTo");
  if (!to) return;
  lsSet("sleuth.updatingTo", null);
  if (to === (await invoke("app_version"))) toast(`Sleuth was updated to ${to}.`);
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
    closeStageMenu(true);
  }
});

// ---------- start ----------
(async function start() {
  renderEmpty();
  $("q").focus();
  if (!(await waitForWorker())) return;
  await Promise.all([loadDrives(), loadSettings(), loadStatus()]).catch((e) => toast(String(e)));
  // This PC's name as the office sees it, to tell "listed by this PC" from another PC.
  if (newReader()) call({ req: "monitor" }).then((r) => {
    const name = r.monitor?.pcs?.[0]?.name;
    if (name && name !== state.pcName) { state.pcName = name; if (state.lastStatus) useStatus(state.lastStatus); }
  }).catch(() => {});
  // Apply "keep reading in the background" from the first start, not only after a save.
  if (state.settings) invoke("set_autostart", { enabled: !!state.settings.keep_indexing_when_closed }).catch(() => {});
  renderEmpty();
  // loadStatus() re-arms itself: 2 s while working, 10 s when idle.
  sayUpdated().catch(() => {});
  setTimeout(autoCheckUpdate, 10000);
  setInterval(autoCheckUpdate, UPDATE_EVERY);
})();
