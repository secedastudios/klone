import {
  T, invoke, listen, $, $$, esc, bytes, ibytes, speed, speedParts, timecode, humanEta, date, kind, icon,
  drawScope, stripHTML, stateLabel, markSVG,
} from './common.js';

// ───────────────────────── state ─────────────────────────

const state = {
  view: 'library',          // library | vault | transfers | settings
  lib: null,                // { name, path }
  cwd: null,                // local folder being browsed
  vaultBox: null,           // storage box id being browsed
  vaultPath: null,          // remote folder being browsed
  usage: {},                // box id → { used, total } | { error }
  rows: [],                 // rendered rows for the current listing
  loading: false,
  selection: new Set(),     // row keys
  anchor: null,             // for shift-click ranges
  sort: { key: 'name', dir: 1 },
  settings: null,
  libraries: [],
  snapshot: { jobs: [], stats: {} },
  transferTab: 'active',
  online: null,
};

let listToken = 0;

const boxes = () => state.settings.boxes;
const boxById = (id) => boxes().find((b) => b.id === id);
const multiBox = () => boxes().length > 1;
const isSetUp = (b) => !!(b.host?.trim() && b.user?.trim());

// ───────────────────────── boot ─────────────────────────

hydrateIcons(document);
await refreshSettings();
state.version = await invoke('app_version');
await refreshLibraries();
state.snapshot = await invoke('snapshot');
if (!boxes().some(isSetUp)) go('settings');
else if (state.libraries.length) openLibrary(state.libraries[0]);
else go('library');
refreshUsage();
setInterval(refreshUsage, 60_000);
setInterval(refreshLibraries, 5_000);

listen('snapshot', ({ payload }) => onSnapshot(payload));
listen('update-available', ({ payload }) => showUpdate(payload));
listen('update-progress', ({ payload }) => {
  const pill = $('#update-pill');
  const pct = payload.total ? Math.floor((payload.downloaded / payload.total) * 100) : null;
  $('.text', pill).textContent = pct === null ? `Downloading ${bytes(payload.downloaded)}…` : `Downloading… ${pct}%`;
});
$('#update-pill').addEventListener('click', () => state.update && offerUpdate(state.update));
listen('menu-check-updates', () => checkForUpdates());
listen('menu-settings', () => go('settings'));

$$('[data-view]').forEach((b) => b.addEventListener('click', () => {
  const v = b.dataset.view;
  if (v === 'vault') openVault(state.vaultBox || boxes()[0].id);
  else go(v);
}));
$('#add-library').addEventListener('click', addLibrary);
$('#open-transfers').addEventListener('click', () => go('transfers'));
$('#toggle-all').addEventListener('click', () => {
  const s = state.snapshot.stats;
  invoke(s.active + s.queued > 0 ? 'pause_all' : 'resume_all');
});
window.addEventListener('resize', () => drawScope($('#scope'), state.snapshot.stats.history));
document.addEventListener('keydown', onKey);
setupDrop();

// ───────────────────────── navigation ─────────────────────────

function go(view) {
  state.view = view;
  state.selection.clear();
  $$('.nav-item').forEach((n) => n.classList.toggle('active',
    (n.dataset.view && n.dataset.view === view) ||
    (view === 'library' && n.dataset.lib === state.lib?.name) ||
    (view === 'vault' && n.dataset.box === state.vaultBox)));
  render();
}

function openLibrary(lib, sub) {
  state.lib = lib;
  state.cwd = sub || lib.path;
  go('library');
  loadLocal();
}

function openVault(boxId, path) {
  const b = boxById(boxId) || boxes()[0];
  if (state.vaultBox !== b.id) state.vaultPath = null;
  state.vaultBox = b.id;
  state.vaultPath = path || state.vaultPath || b.remote_root;
  go('vault');
  loadVault();
}

function render() {
  updateSelectionBar();
  switch (state.view) {
    case 'library': return renderLibrary();
    case 'vault': return renderVault();
    case 'transfers': return renderTransfers();
    case 'settings': return renderSettings();
  }
}

// ───────────────────────── data ─────────────────────────

async function refreshSettings() {
  state.settings = await invoke('get_settings');
}

async function refreshLibraries() {
  const before = state.libraries.find((l) => l.name === state.lib?.name)?.online;
  state.libraries = await invoke('libraries');
  const cur = state.libraries.find((l) => l.name === state.lib?.name);
  if (cur) state.lib = cur;
  if (cur && before !== undefined && before !== cur.online && state.view === 'library') loadLocal();
  const ul = $('#libraries');
  ul.innerHTML = state.libraries.map((l) => {
    const usedPct = l.total ? ((l.total - l.free) / l.total) * 100 : 0;
    return `<li class="lib-item ${l.online ? '' : 'offline'}">
      <button class="nav-item ${state.view === 'library' && state.lib?.name === l.name ? 'active' : ''}" data-lib="${esc(l.name)}" type="button" title="${esc(l.path)}">
        <span class="ico">${icon.drive}</span><span class="label">${esc(l.name)}</span>
      </button>
      <div class="lib-sub">${l.online
        ? `<span class="minibar ${usedPct > 90 ? 'tight' : ''}"><i style="--w:${usedPct.toFixed(1)}%"></i></span><span>${bytes(l.free)} free</span>`
        : '<span>offline · not mounted</span>'}</div>
    </li>`;
  }).join('');
  $$('[data-lib]', ul).forEach((b) => {
    const lib = state.libraries.find((l) => l.name === b.dataset.lib);
    b.addEventListener('click', () => openLibrary(lib));
    b.addEventListener('contextmenu', (e) => { e.preventDefault(); confirmRemoveLibrary(lib); });
  });
}

// ───────────────────────── updates ─────────────────────────

function showUpdate(info) {
  state.update = info;
  const pill = $('#update-pill');
  pill.hidden = false;
  pill.disabled = false;
  $('.text', pill).textContent = `Update to ${info.version}`;
  pill.title = `Klone ${info.version} is available (you have ${info.current})`;
}

/** Klone → Check for Updates…: always answers, like any Mac app. */
async function checkForUpdates() {
  try {
    const u = await invoke('check_update');
    if (u) { showUpdate(u); offerUpdate(u); return; }
    modal(`<div class="dialog"><h2>You’re up to date</h2><p>Klone ${esc(state.version)} is the newest version.</p>
      <div class="actions"><button class="btn primary" data-c="ok">OK</button></div></div>`);
  } catch (e) {
    modal(`<div class="dialog"><h2>Couldn’t check for updates</h2><p>${esc(String(e))}</p>
      <div class="actions"><button class="btn primary" data-c="ok">OK</button></div></div>`);
  }
}

function offerUpdate(info) {
  const s = state.snapshot.stats;
  const busy = s.active + s.queued;
  modal(`<div class="dialog"><h2>Update to Klone <em>${esc(info.version)}</em></h2>
    <p>You have ${esc(info.current)}. Klone downloads the update, checks its signature, and restarts.${busy ? ` <b>${s.active ? `${s.active} transfer${s.active === 1 ? ' is' : 's are'} running` : `${s.queued} queued`}</b> — they pick up where they left off after the restart.` : ''}</p>
    ${info.notes ? `<pre class="update-notes">${esc(info.notes)}</pre>` : ''}
    <div class="actions"><button class="btn ghost" data-c="cancel">Later</button><button class="btn primary" data-c="ok">Restart &amp; update</button></div></div>`,
  async (c) => {
    if (c !== 'ok') return;
    const pill = $('#update-pill');
    pill.hidden = false; pill.disabled = true;
    $('.text', pill).textContent = 'Downloading…';
    try { await invoke('install_update'); }
    catch (e) { toast(String(e), true); showUpdate(info); }
  });
}

function renderBoxes() {
  const ul = $('#boxes');
  ul.innerHTML = boxes().map((b) => {
    const u = state.usage[b.id];
    const pct = u?.total ? (u.used / u.total) * 100 : 0;
    const sub = !u ? '<span class="used">checking…</span><span class="of"></span>'
      : u.setup ? '<span class="used">not set up</span><span class="of"></span>'
      : u.error ? '<span class="used err">unreachable</span><span class="of"></span>'
      : `<span class="used">${ibytes(u.used)} used</span><span class="of">of ${ibytes(u.total, 0)}</span>`;
    return `<li class="box-item ${u?.error ? 'offline' : ''}">
      <button class="nav-item ${state.view === 'vault' && state.vaultBox === b.id ? 'active' : ''}" data-box="${esc(b.id)}" type="button" title="${isSetUp(b) ? `${esc(b.user)}@${esc(b.host)}` : 'Not set up yet'}">
        <span class="ico">${icon.vault}</span><span class="label">${esc(b.name)}</span>
      </button>
      <div class="meter"><div class="meter-bar ${pct > 90 ? 'tight' : ''}"><i style="--w:${Math.max(pct, 0.6).toFixed(1)}%"></i></div><div class="meter-text">${sub}</div></div>
    </li>`;
  }).join('');
  $$('[data-box]', ul).forEach((btn) => btn.addEventListener('click', () => {
    const b = boxById(btn.dataset.box);
    if (b && !isSetUp(b)) { go('settings'); toast(`Add ${b.name}’s server and username to start using it`); return; }
    openVault(btn.dataset.box);
  }));
}

async function refreshUsage() {
  renderBoxes();
  await Promise.all(boxes().map(async (b) => {
    if (!isSetUp(b)) { state.usage[b.id] = { error: 'Not set up yet — add its server and username in Settings', setup: true }; return; }
    try { state.usage[b.id] = await invoke('vault_usage', { boxId: b.id }); }
    catch (e) { state.usage[b.id] = { error: String(e) }; }
  }));
  renderBoxes();
  const list = boxes();
  const up = list.filter((b) => !state.usage[b.id]?.error);
  const el = $('#conn-status');
  state.online = up.length > 0;
  el.className = `conn-status ${up.length === list.length ? 'ok' : up.length ? 'warn' : 'err'}`;
  el.title = list.filter((b) => state.usage[b.id]?.error).map((b) => `${b.name}: ${state.usage[b.id].error}`).join('\n') || 'All storage boxes reachable';
  el.querySelector('.text').textContent = !list.some(isSetUp) ? 'Set up a storage box'
    : list.length === 1
    ? (up.length ? `${list[0].user} · online` : 'Storage box unreachable')
    : `${up.length} of ${list.length} boxes online`;
}

async function loadLocal() {
  const token = ++listToken;
  state.loading = true;
  state.rows = [];
  if (state.view === 'library') renderLibrary();
  let local;
  try {
    local = await invoke('list_local', { path: state.cwd });
  } catch (e) {
    toast(String(e), true);
    return;
  }
  if (token !== listToken) return;
  // Show local files immediately; vault status fills in after.
  state.localMissing = !local.exists;
  state.rows = mergeRows(local, null);
  state.loading = false;
  if (state.view === 'library') renderLibrary();
  const remotes = {};
  await Promise.all(boxes().map(async (b) => {
    try { remotes[b.id] = await invoke('list_remote', { boxId: b.id, path: local.remote_dirs[b.id] }); }
    catch { remotes[b.id] = null; }
  }));
  if (token !== listToken) return;
  state.rows = mergeRows(local, remotes);
  state.vaultReachable = Object.values(remotes).some(Boolean);
  if (state.view === 'library') renderLibrary();
}

/**
 * Merge a local listing with the same folder in every box.
 * `remotes`: box id → listing (null = unreachable), or null while loading.
 * Each row gets `copies`: box id → { path, size } for boxes holding it.
 */
function mergeRows(local, remotes) {
  const known = remotes ? boxes().filter((b) => remotes[b.id]) : [];
  const copiesOf = (name) => {
    const c = {};
    for (const b of known) {
      const e = remotes[b.id].entries.find((x) => x.name === name);
      if (e) c[b.id] = { path: e.path, size: e.size, is_dir: e.is_dir };
    }
    return c;
  };
  const jobsIn = (name) => known.flatMap((b) => remotes[b.id].jobs.filter(([p]) => p.endsWith('/' + name)).map(([, id]) => id));
  const seen = new Set();
  const firstBox = boxes()[0].id;
  const rows = local.entries.map((e) => {
    seen.add(e.name);
    const copies = copiesOf(e.name);
    const ids = Object.keys(copies);
    let status = known.length ? 'local' : 'unknown';
    if (ids.length && !e.is_dir) status = ids.some((id) => copies[id].size !== e.size) ? 'differs' : 'synced';
    if (ids.length && e.is_dir) status = 'folder-vault';
    if (!ids.length && e.is_dir) status = known.length ? 'folder-local' : 'unknown';
    return { key: 'l:' + e.path, name: e.name, is_dir: e.is_dir, size: e.size, mtime: e.mtime, local: e.path,
      copies, remote: copies[ids[0]]?.path || joinRemote(local.remote_dirs[firstBox], e.name), status, jobs: e.jobs || [] };
  });
  // Vault-only items appear as ghosts so offloaded media stays discoverable.
  for (const b of known) {
    for (const r of remotes[b.id].entries) {
      if (seen.has(r.name)) continue;
      seen.add(r.name);
      const copies = copiesOf(r.name);
      rows.push({ key: 'r:' + r.name, name: r.name, is_dir: r.is_dir, size: r.size, mtime: r.mtime, local: joinLocal(local.path, r.name),
        copies, remote: r.path, status: 'vault', ghost: true, jobs: jobsIn(r.name) });
    }
  }
  return rows;
}

async function loadVault() {
  const token = ++listToken;
  state.loading = true;
  state.rows = [];
  state.vaultError = null;
  if (state.view === 'vault') renderVault();
  try {
    const res = await invoke('list_remote', { boxId: state.vaultBox, path: state.vaultPath });
    if (token !== listToken) return;
    state.vaultLocalDir = res.local_dir;
    const jobs = new Map(res.jobs);
    state.rows = res.entries.map((e) => ({ key: 'r:' + e.path, name: e.name, is_dir: e.is_dir, size: e.size, mtime: e.mtime, remote: e.path,
      copies: { [state.vaultBox]: { path: e.path, size: e.size, is_dir: e.is_dir } }, status: e.is_dir ? 'folder-vault' : 'vault', jobs: jobs.has(e.path) ? [jobs.get(e.path)] : [] }));
  } catch (e) {
    state.rows = [];
    state.vaultError = String(e);
  }
  state.loading = false;
  if (token === listToken && state.view === 'vault') renderVault();
}

function reload() {
  if (state.view === 'library') loadLocal();
  else if (state.view === 'vault') loadVault();
}

// ───────────────────────── library view ─────────────────────────

function renderLibrary() {
  const view = $('#view');
  if (!state.lib) {
    setHead('', `Your <em>media</em>, offloaded.`, '');
    view.innerHTML = `<div class="empty">
      ${markSVG('big live')}
      <h2>Free up your drives,<br/><em>keep every frame.</em></h2>
      <p>Add the folders where your projects live. Klone mirrors them into your storage box,
         resumes anything that gets interrupted, and brings media back when you need it.</p>
      <button class="btn primary" id="hero-add" type="button"><span class="ico">${icon.plus}</span>Add a library folder</button>
      <div class="steps">
        <div class="step"><b>01 · ARCHIVE</b>Send footage to the vault. Checksummed end-to-end.</div>
        <div class="step"><b>02 · FREE SPACE</b>Offload removes the local copy only after verification.</div>
        <div class="step"><b>03 · RETRIEVE</b>Vault-only files stay visible. Pull them back anytime.</div>
      </div>
    </div>`;
    $('#hero-add').addEventListener('click', addLibrary);
    $('#toolbar').innerHTML = '';
    return;
  }
  const rel = relPath(state.lib.path, state.cwd);
  const parts = rel ? rel.split('/') : [];
  const crumbs = [`<button data-crumb="">${esc(state.lib.name)}</button>`]
    .concat(parts.map((p, i) => i === parts.length - 1 ? `<span class="here">${esc(p)}</span>` : `<button data-crumb="${esc(parts.slice(0, i + 1).join('/'))}">${esc(p)}</button>`))
    .join('<span class="sep">/</span>');
  setHead(parts.length ? crumbs : `<span class="here">${esc(state.lib.path)}</span>`, esc(parts.at(-1) || state.lib.name), `
    ${parts.length ? `<button class="btn ghost icon-only" id="up-dir" title="Enclosing folder (⌘↑)"><span class="ico">${icon.back}</span></button>` : ''}
    <button class="btn ghost icon-only" id="refresh" title="Refresh"><span class="ico">${icon.refresh}</span></button>
    ${state.localMissing ? '' : `<button class="btn" id="reveal-cwd"><span class="ico">${icon.reveal}</span>Show in Finder</button>
    <button class="btn primary" id="archive-all"><span class="ico">${icon.up}</span>Archive folder</button>`}`);
  $$('[data-crumb]').forEach((b) => b.addEventListener('click', () => {
    state.cwd = b.dataset.crumb ? joinLocal(state.lib.path, b.dataset.crumb) : state.lib.path;
    state.selection.clear(); loadLocal();
  }));
  $('#up-dir')?.addEventListener('click', goUp);
  $('#refresh').addEventListener('click', reload);
  $('#reveal-cwd')?.addEventListener('click', () => invoke('reveal', { path: state.cwd }));
  $('#archive-all')?.addEventListener('click', () => archiveFlow([state.cwd], state.rows.filter((r) => !r.ghost), 'ask'));

  const notice = state.localMissing ? `<div class="notice">
      <span class="ico">${icon.drive}</span>
      <div><b>${esc(driveName(state.cwd) || state.lib.name)} isn't connected.</b>
      <span>Showing what's already in your storage ${multiBox() ? 'boxes' : 'box'}. Plug the drive in to see local files — transfers for it wait and continue on their own.</span></div>
    </div>` : '';
  view.innerHTML = notice + (state.localMissing && !state.loading && !state.rows.length
    ? `<div class="empty"><h2>Drive <em>offline</em></h2><p>Nothing from this folder is in the vault yet.</p></div>`
    : tableHTML('library'));
  bindTable();
}

// ───────────────────────── vault view ─────────────────────────

function renderVault() {
  const box = boxById(state.vaultBox) || boxes()[0];
  const root = box.remote_root;
  const rel = relPath(root, state.vaultPath);
  const parts = rel ? rel.split('/') : [];
  const crumbs = [`<button data-vcrumb="">${esc(box.name)}</button>`]
    .concat(parts.map((p, i) => i === parts.length - 1 ? `<span class="here">${esc(p)}</span>` : `<button data-vcrumb="${esc(parts.slice(0, i + 1).join('/'))}">${esc(p)}</button>`))
    .join('<span class="sep">/</span>');
  setHead(parts.length ? crumbs : `<span class="here">${esc(box.user)}@${esc(box.host)}:${esc(root)}</span>`, parts.length ? esc(parts.at(-1)) : esc(box.name), `
    ${parts.length ? `<button class="btn ghost icon-only" id="up-dir" title="Enclosing folder (⌘↑)"><span class="ico">${icon.back}</span></button>` : ''}
    <button class="btn ghost icon-only" id="refresh" title="Refresh"><span class="ico">${icon.refresh}</span></button>
    <button class="btn" id="new-folder"><span class="ico">${icon.plus}</span>New folder</button>`);
  $$('[data-vcrumb]').forEach((b) => b.addEventListener('click', () => {
    state.vaultPath = b.dataset.vcrumb ? `${root}/${b.dataset.vcrumb}` : root;
    state.selection.clear(); loadVault();
  }));
  $('#up-dir')?.addEventListener('click', goUp);
  $('#refresh').addEventListener('click', reload);
  $('#new-folder').addEventListener('click', newVaultFolder);

  const view = $('#view');
  if (!state.loading && state.vaultError && !state.rows.length) {
    view.innerHTML = `<div class="empty"><h2>Can't reach <em>${esc(box.name)}</em></h2><p>${esc(state.vaultError)}</p>
      <button class="btn" id="retry-vault"><span class="ico">${icon.retry}</span>Try again</button></div>`;
    $('#retry-vault').addEventListener('click', () => { state.vaultError = null; loadVault(); });
    return;
  }
  if (!state.loading && !state.rows.length) {
    view.innerHTML = `<div class="empty">${markSVG('big live')}<h2>Nothing here <em>yet</em></h2>
      <p>${parts.length ? 'This folder is empty.' : 'Archive something from a library — or drop files anywhere on this window — and it will appear here, organised by library and folder.'}</p></div>`;
    return;
  }
  view.innerHTML = tableHTML('vault');
  bindTable();
}

// ───────────────────────── shared table ─────────────────────────

function sortedRows() {
  const { key, dir } = state.sort;
  return [...state.rows].sort((a, b) => {
    if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
    const va = a[key], vb = b[key];
    const c = typeof va === 'string' ? va.localeCompare(vb, undefined, { numeric: true, sensitivity: 'base' }) : (va || 0) - (vb || 0);
    return c * dir;
  });
}

function tableHTML(mode) {
  if (state.loading) {
    return `<table class="table"><tbody>${Array.from({ length: 8 }, (_, i) =>
      `<tr class="skeleton"><td><div style="width:${40 + ((i * 37) % 40)}%"></div></td><td><div style="width:60px"></div></td><td><div style="width:110px"></div></td><td><div style="width:70px"></div></td><td></td></tr>`).join('')}</tbody></table>`;
  }
  if (!state.rows.length) {
    return `<div class="empty"><h2>An <em>empty</em> folder</h2><p>No media in this folder.</p></div>`;
  }
  const rows = sortedRows();
  const th = (k, label, cls = '') => {
    const active = state.sort.key === k;
    return `<th class="sortable ${cls}" data-sort="${k}">${label}${active ? `<span class="arrow">${state.sort.dir > 0 ? '↑' : '↓'}</span>` : ''}</th>`;
  };
  const files = rows.filter((r) => !r.is_dir);
  const inVault = files.filter((r) => r.status === 'synced' || r.status === 'vault');
  const localOnly = files.filter((r) => r.status === 'local' || r.status === 'differs');
  const summary = mode === 'library'
    ? `<div class="list-summary"><span><b>${files.length}</b> files</span><span><b>${bytes(files.filter((r) => !r.ghost).reduce((a, r) => a + r.size, 0))}</b> on this Mac</span><span><b>${inVault.length}</b> in vault</span>${localOnly.length ? `<span><b>${localOnly.length}</b> not yet archived · ${bytes(localOnly.reduce((a, r) => a + r.size, 0))}</span>` : ''}${state.vaultReachable === false ? '<span style="color:var(--red)">vault status unavailable</span>' : ''}</div>`
    : `<div class="list-summary"><span><b>${files.length}</b> files</span><span><b>${bytes(files.reduce((a, r) => a + r.size, 0))}</b> in this folder</span></div>`;
  return `${summary}<table class="table">
    <thead><tr>${th('name', 'Name')}${th('size', 'Size', 'num')}${th('mtime', 'Modified')}<th>Status</th><th></th></tr></thead>
    <tbody class="stagger">${rows.map((r) => rowHTML(r, mode)).join('')}</tbody></table>`;
}

function activeJob(r) {
  return (r.jobs || []).map((id) => state.snapshot.jobs.find((j) => j.id === id))
    .find((j) => j && !['done', 'canceled'].includes(j.state));
}

function movingPill(job) {
  const pct = job.size ? Math.floor((job.done / job.size) * 100) : 0;
  const label = job.state === 'running' ? `${pct}%` : stateLabel[job.state].toLowerCase();
  const where = multiBox() && job.box_name ? ` · ${esc(job.box_name)}` : '';
  return `<span class="pill moving ${job.direction === 'download' ? 'down' : ''}"><span class="fill" style="--w:${pct}%"></span>${job.direction === 'download' ? '↓' : '↑'} ${label}${where}</span>`;
}

/** One chip per box: lit when that box holds a copy. */
function boxChips(r) {
  return `<span class="pill boxes">${boxes().map((b) => {
    const c = r.copies?.[b.id];
    const cls = !c ? '' : (!r.is_dir && !r.ghost && c.size !== r.size) ? 'diff' : 'on';
    return `<i class="${cls}" title="${esc(b.name)}: ${!c ? 'not archived' : cls === 'diff' ? 'different version' : 'archived'}">${esc(b.name)}</i>`;
  }).join('')}</span>`;
}

function rowHTML(r, mode) {
  const k = kind(r.name, r.is_dir);
  const sel = state.selection.has(r.key) ? 'sel' : '';
  const job = activeJob(r);
  let status;
  if (job) {
    status = movingPill(job);
  } else if (mode === 'library' && multiBox() && !['local', 'folder-local', 'unknown'].includes(r.status)) {
    status = (r.status === 'differs' ? `<span class="pill differs">changed locally</span> ` : '') + boxChips(r);
  } else {
    status = {
      synced: `<span class="pill synced">in vault</span>`,
      local: `<span class="pill local">this Mac only</span>`,
      differs: `<span class="pill differs">changed locally</span>`,
      vault: mode === 'vault' ? '' : `<span class="pill vault">vault only</span>`,
      'folder-vault': mode === 'vault' ? '' : `<span class="pill mixed">in vault</span>`,
      'folder-local': `<span class="pill local">this Mac only</span>`,
      unknown: '',
    }[r.status] || '';
  }
  const canUp = mode === 'library' && !r.ghost;
  const canDown = mode === 'vault' || r.ghost;
  const actions = [
    canUp ? `<button class="up" data-act="archive" title="Archive">${icon.up}</button>` : '',
    canUp && !r.is_dir ? `<button class="up" data-act="offload" title="Archive & free space">${icon.free}</button>` : '',
    canDown ? `<button class="down" data-act="retrieve" title="Retrieve to this Mac">${icon.down}</button>` : '',
    !r.ghost && mode === 'library' ? `<button data-act="reveal" title="Show in Finder">${icon.reveal}</button>` : '',
  ].join('');
  return `<tr class="${sel} ${r.ghost ? 'ghost' : ''} ${r.is_dir ? 'dir' : ''}" data-key="${esc(r.key)}">
    <td><div class="name-cell"><span class="glyph ${k}">${icon[k]}</span><span class="nm" title="${esc(r.name)}">${esc(r.name)}</span></div></td>
    <td class="num meta">${r.is_dir ? '<span style="color:var(--faint)">—</span>' : bytes(r.size)}</td>
    <td class="meta dim">${date(r.mtime)}</td>
    <td>${status}</td>
    <td><div class="row-actions">${actions}</div></td>
  </tr>`;
}

function bindTable() {
  const view = $('#view');
  $$('[data-sort]', view).forEach((th) => th.addEventListener('click', () => {
    const k = th.dataset.sort;
    state.sort = { key: k, dir: state.sort.key === k ? -state.sort.dir : (k === 'name' ? 1 : -1) };
    render();
  }));
  $$('tbody tr[data-key]', view).forEach((tr) => {
    const row = () => state.rows.find((r) => r.key === tr.dataset.key);
    tr.addEventListener('click', (e) => {
      const btn = e.target.closest('[data-act]');
      if (btn) return rowAction(btn.dataset.act, row());
      select(tr.dataset.key, e);
    });
    tr.addEventListener('dblclick', () => openRow(row()));
  });
}

function openRow(r) {
  if (!r) return;
  if (r.is_dir) {
    state.selection.clear();
    if (state.view === 'library') { state.cwd = r.local; loadLocal(); }
    else { state.vaultPath = r.remote; loadVault(); }
  } else if (state.view === 'library' && !r.ghost) {
    invoke('reveal', { path: r.local });
  }
}

function rowAction(act, r) {
  if (!r) return;
  if (act === 'archive') return archiveFlow([r.local], [r], 'keep');
  if (act === 'offload') return archiveFlow([r.local], [r], 'free');
  if (act === 'retrieve') return doRetrieve([r]);
  if (act === 'reveal') return invoke('reveal', { path: r.local });
}

function select(key, e) {
  const keys = sortedRows().map((r) => r.key);
  if (e.shiftKey && state.anchor) {
    const [a, b] = [keys.indexOf(state.anchor), keys.indexOf(key)].sort((x, y) => x - y);
    if (!e.metaKey) state.selection.clear();
    keys.slice(a, b + 1).forEach((k) => state.selection.add(k));
  } else if (e.metaKey) {
    state.selection.has(key) ? state.selection.delete(key) : state.selection.add(key);
    state.anchor = key;
  } else {
    const only = state.selection.size === 1 && state.selection.has(key);
    state.selection.clear();
    if (!only) state.selection.add(key);
    state.anchor = key;
  }
  $$('tbody tr[data-key]').forEach((tr) => tr.classList.toggle('sel', state.selection.has(tr.dataset.key)));
  updateSelectionBar();
}

function selectedRows() {
  return state.rows.filter((r) => state.selection.has(r.key));
}

function updateSelectionBar() {
  const bar = $('#selection-bar');
  const rows = selectedRows();
  if (!rows.length || !['library', 'vault'].includes(state.view)) { bar.hidden = true; return; }
  const size = rows.reduce((a, r) => a + (r.is_dir ? 0 : r.size), 0);
  const local = rows.filter((r) => !r.ghost && state.view === 'library');
  const remote = rows.filter((r) => state.view === 'vault' || r.ghost);
  bar.hidden = false;
  bar.innerHTML = `
    <span class="info"><b>${rows.length}</b> selected${size ? ` · ${bytes(size)}` : ''}</span>
    ${local.length ? `<button class="btn" data-sb="archive"><span class="ico">${icon.up}</span>Archive</button>
      <button class="btn primary" data-sb="offload"><span class="ico">${icon.free}</span>Archive &amp; free space</button>` : ''}
    ${remote.length ? `<button class="btn down" data-sb="retrieve"><span class="ico">${icon.down}</span>Retrieve${local.length ? ` ${remote.length}` : ''}</button>
      <button class="btn" data-sb="retrieve-to">Retrieve to…</button>` : ''}
    ${state.view === 'vault' ? `<span class="divider"></span><button class="btn ghost danger icon-only" data-sb="delete" title="Delete from vault">${icon.trash}</button>` : ''}
    <span class="divider"></span>
    <button class="btn ghost icon-only" data-sb="clear" title="Clear selection (esc)">${icon.x}</button>`;
  $$('[data-sb]', bar).forEach((b) => b.addEventListener('click', () => {
    const a = b.dataset.sb;
    if (a === 'archive') archiveFlow(local.map((r) => r.local), local, 'keep');
    if (a === 'offload') archiveFlow(local.map((r) => r.local), local, 'free');
    if (a === 'retrieve') doRetrieve(remote);
    if (a === 'retrieve-to') retrieveTo(remote);
    if (a === 'delete') confirmDelete(rows);
    if (a === 'clear') { state.selection.clear(); render(); }
  }));
}

// ───────────────────────── actions ─────────────────────────

async function doArchive(paths, offload, targets) {
  try {
    const n = await invoke('archive', { paths, offload, targets });
    const where = targets.map((id) => boxById(id)?.name).filter(Boolean).join(' + ');
    toast(n ? `${n} transfer${n === 1 ? '' : 's'} queued${multiBox() ? ` to ${where}` : ''}${offload ? ' — local copies freed after verification' : ''}`
      : offload ? 'Already queued — will free space once verified' : 'Already queued');
    if (targets.length) state.settings.last_targets = targets;
    state.selection.clear();
    setTimeout(reload, 400);
  } catch (e) { toast(String(e), true); }
}

/** Where a retrieve should come from: the vault being browsed, else the first box holding a copy. */
function sourceBox(r) {
  if (state.view === 'vault') return state.vaultBox;
  return boxes().find((b) => r.copies?.[b.id])?.id || boxes()[0].id;
}

async function doRetrieve(rows, dest = null) {
  const byBox = new Map();
  for (const r of rows) {
    const id = sourceBox(r);
    if (!byBox.has(id)) byBox.set(id, []);
    byBox.get(id).push(r.copies?.[id]?.path || r.remote);
  }
  try {
    let n = 0;
    for (const [boxId, paths] of byBox) n += await invoke('retrieve', { boxId, paths, dest });
    toast(n ? `Retrieving ${n} file${n === 1 ? '' : 's'}` : 'Already on the way');
    state.selection.clear();
    setTimeout(reload, 400);
  } catch (e) { toast(String(e), true); }
}

async function retrieveTo(rows) {
  const dest = await T.dialog.open({ directory: true, title: 'Retrieve to…' });
  if (dest) doRetrieve(rows, dest);
}

function defaultTargets() {
  const last = (state.settings.last_targets || []).filter((id) => boxById(id));
  return last.length ? last : [boxes()[0].id];
}

/**
 * Archive `paths`. mode: 'ask' (keep or free), 'keep', or 'free' (confirm).
 * With several storage boxes the dialog also asks where to send them.
 */
function archiveFlow(paths, rows, mode, title) {
  const single = !multiBox();
  if (single && mode === 'keep') return doArchive(paths, false, [boxes()[0].id]);
  const size = rows.reduce((a, r) => a + (r.is_dir ? 0 : r.size || 0), 0);
  const what = title || (paths.length === 1 ? basename(paths[0]) : `${paths.length} items`);
  const chosen = new Set(defaultTargets());
  const freeWord = state.settings.offload_mode === 'trash' ? 'moved to the Trash' : '<b>permanently deleted</b>';
  const picker = single ? '' : `<div class="box-picker">${boxes().map((b) => {
    const u = state.usage[b.id];
    const meta = u?.error ? 'unreachable' : u?.total ? `${ibytes(u.total - u.used)} free` : '';
    return `<label class="box-pick ${u?.error ? 'offline' : ''}"><input type="checkbox" value="${esc(b.id)}" ${chosen.has(b.id) ? 'checked' : ''}/>
      <span class="tick"></span><span class="ico">${icon.vault}</span><span class="nm">${esc(b.name)}</span><span class="meta">${meta}</span></label>`;
  }).join('')}</div>`;
  const body = {
    ask: `<p>${size ? `${bytes(size)} · ` : ''}Large files are split across parallel streams and resume automatically after any interruption.</p>
      ${single ? '' : '<h4>Send to</h4>'}${picker}
      <div class="choices">
        <button class="choice" data-c="keep"><span class="ico">${icon.up}</span><span><b>Archive</b><span>Copy and keep the files on this Mac.</span></span></button>
        <button class="choice" data-c="free"><span class="ico">${icon.free}</span><span><b>Archive &amp; free space</b><span>Remove local copies once ${single ? 'the vault checksum matches' : 'every chosen box has a verified copy'} (${state.settings.offload_mode === 'trash' ? 'moved to Trash' : 'deleted permanently'}).</span></span></button>
      </div>
      <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button></div>`,
    keep: `<p>${size ? `${bytes(size)} · ` : ''}Choose which storage boxes get a copy.</p><h4>Send to</h4>${picker}
      <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button><button class="btn primary" data-c="keep">Archive</button></div>`,
    free: `<p>${rows.length} item${rows.length === 1 ? '' : 's'} will be uploaded and verified with an MD5 checksum on ${single ? 'the storage box' : '<b>every chosen box</b>'}, and only then ${freeWord} from this Mac. They stay visible here as <i>vault only</i>.</p>
      ${single ? '' : '<h4>Send to</h4>'}${picker}
      <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button><button class="btn primary" data-c="free">Archive &amp; free space</button></div>`,
  }[mode];
  const heading = mode === 'free' ? `Archive &amp; free <em>${bytes(size) || 'space'}</em>?` : single ? `Send <em>${esc(what)}</em> to the vault` : `Archive <em>${esc(what)}</em>`;
  let targets = [...chosen];
  modal(`<div class="dialog">
    <h2>${heading}</h2>${body}
  </div>`, (c) => {
    if (c === 'cancel' || !targets.length) return;
    if (c === 'keep') doArchive(paths, false, targets);
    if (c === 'free') doArchive(paths, true, targets);
  }, () => {
    const sync = () => {
      targets = $$('.box-picker input:checked').map((i) => i.value);
      if (single) targets = [boxes()[0].id];
      $$('#modal [data-c="keep"], #modal [data-c="free"]').forEach((b) => { b.disabled = !targets.length; });
    };
    $$('.box-picker input').forEach((i) => i.addEventListener('change', sync));
    sync();
  });
}

function confirmDelete(rows) {
  const confirmWord = 'delete';
  modal(`<div class="dialog">
    <h2>Delete from ${esc(boxById(state.vaultBox)?.name || 'the vault')}?</h2>
    <p>${rows.length} item${rows.length === 1 ? '' : 's'} ${rows.some((r) => r.is_dir) ? '(including everything inside folders) ' : ''}will be permanently removed from this storage box${multiBox() ? ' (other boxes are not affected)' : ''}. Copies on this Mac are not affected. This can't be undone.</p>
    <input class="input" id="confirm-word" placeholder="Type “${confirmWord}” to confirm" autocomplete="off" />
    <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button><button class="btn danger solid" data-c="ok" id="confirm-del" disabled>Delete forever</button></div>
  </div>`, async (c) => {
    if (c !== 'ok') return;
    try {
      await invoke('vault_delete', { boxId: state.vaultBox, paths: rows.map((r) => r.remote) });
      toast(`Deleted ${rows.length} item${rows.length === 1 ? '' : 's'} from ${boxById(state.vaultBox)?.name || 'the vault'}`);
      state.selection.clear();
      reload(); refreshUsage();
    } catch (e) { toast(String(e), true); }
  });
  const input = $('#confirm-word');
  input.focus();
  input.addEventListener('input', () => { $('#confirm-del').disabled = input.value.trim().toLowerCase() !== confirmWord; });
}

function newVaultFolder() {
  modal(`<div class="dialog"><h2>New vault folder</h2><p>Created inside <span style="font-family:var(--mono);font-size:11px">${esc(state.vaultPath)}</span></p>
    <input class="input" id="folder-name" placeholder="Folder name" autocomplete="off" />
    <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button><button class="btn primary" data-c="ok">Create</button></div></div>`,
  async (c, form) => {
    if (c !== 'ok') return;
    const name = (form['folder-name'] || '').trim();
    if (!name) return;
    if (name.includes('/') || name === '.' || name === '..') { toast('Folder names can’t contain “/”', true); return; }
    try { await invoke('vault_mkdir', { boxId: state.vaultBox, path: joinRemote(state.vaultPath, name) }); loadVault(); }
    catch (e) { toast(String(e), true); }
  }, () => $('#folder-name').focus());
}

async function addLibrary() {
  const path = await T.dialog.open({ directory: true, title: 'Choose a folder to manage with Klone' });
  if (!path) return;
  try {
    state.settings = await invoke('add_library', { path });
    await refreshLibraries();
    const lib = state.libraries.find((l) => l.path === path);
    if (lib) openLibrary(lib);
    toast(`Added ${lib?.name} — archives to ${lib?.name}/ in your storage ${multiBox() ? 'boxes' : 'box'}`);
  } catch (e) { toast(String(e), true); }
}

function confirmRemoveLibrary(lib) {
  modal(`<div class="dialog"><h2>Remove “${esc(lib.name)}”?</h2>
    <p>Klone will stop showing this folder. Nothing is deleted — not on this Mac, not in the vault.</p>
    <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button><button class="btn" data-c="ok">Remove library</button></div></div>`,
  async (c) => {
    if (c !== 'ok') return;
    state.settings = await invoke('remove_library', { name: lib.name });
    await refreshLibraries();
    if (state.lib?.name === lib.name) { state.lib = null; state.libraries[0] ? openLibrary(state.libraries[0]) : go('library'); }
  });
}

function goUp() {
  if (state.view === 'library' && state.cwd !== state.lib.path) {
    state.cwd = state.cwd.replace(/\/[^/]+\/?$/, '') || '/';
    state.selection.clear(); loadLocal();
  } else if (state.view === 'vault' && state.vaultPath !== boxById(state.vaultBox)?.remote_root) {
    state.vaultPath = state.vaultPath.replace(/\/[^/]+\/?$/, '') || '/';
    state.selection.clear(); loadVault();
  }
}

// ───────────────────────── transfers view ─────────────────────────

function renderTransfers() {
  const jobs = state.snapshot.jobs;
  const groups = {
    active: jobs.filter((j) => ['running', 'verifying', 'retrying', 'queued', 'paused', 'waiting'].includes(j.state)),
    failed: jobs.filter((j) => j.state === 'failed'),
    done: jobs.filter((j) => ['done', 'canceled'].includes(j.state)),
  };
  const s = state.snapshot.stats;
  setHead(`<span class="here">${s.active} moving · ${s.queued} queued · ${s.paused} paused${s.waiting ? ` · ${s.waiting} waiting for ${(s.offline_drives || []).join(', ') || 'a drive'}` : ''}</span>`, `Transfers`, `
    ${groups.done.length ? `<button class="btn ghost" id="clear-done">Clear finished</button>` : ''}
    ${s.paused || s.failed ? `<button class="btn" id="resume-all"><span class="ico">${icon.play}</span>Resume all</button>` : ''}
    ${s.active + s.queued ? `<button class="btn" id="pause-all"><span class="ico">${icon.pause}</span>Pause all</button>` : ''}`);
  $('#clear-done')?.addEventListener('click', () => invoke('clear_finished'));
  $('#resume-all')?.addEventListener('click', () => invoke('resume_all'));
  $('#pause-all')?.addEventListener('click', () => invoke('pause_all'));

  const tab = state.transferTab;
  const list = groups[tab];
  const view = $('#view');
  const tabs = `<div class="tabs">
    ${['active', 'failed', 'done'].map((t) => `<button class="tab ${t === tab ? 'active' : ''}" data-tab="${t}">${{ active: 'In progress', failed: 'Needs attention', done: 'Finished' }[t]}<span class="n">${groups[t].length}</span></button>`).join('')}
  </div>`;
  if (!list.length) {
    view.innerHTML = tabs + `<div class="empty"><h2>${tab === 'active' ? 'All <em>quiet</em>' : tab === 'failed' ? 'Nothing <em>broken</em>' : 'No history <em>yet</em>'}</h2>
      <p>${tab === 'active' ? 'Archive footage from a library or drop files on this window to start.' : tab === 'failed' ? 'Transfers that cannot recover on their own show up here.' : 'Completed transfers are listed here.'}</p></div>`;
  } else {
    // Group by batch, newest first for history.
    const batches = new Map();
    const ordered = tab === 'done' ? [...list].sort((a, b) => (b.finished || 0) - (a.finished || 0)) : list;
    for (const j of ordered) {
      if (!batches.has(j.batch)) batches.set(j.batch, []);
      batches.get(j.batch).push(j);
    }
    view.innerHTML = tabs + [...batches.values()].map((js) => {
      const total = js.reduce((a, j) => a + j.size, 0);
      const done = js.reduce((a, j) => a + j.done, 0);
      const dir = js[0].direction;
      const names = [...new Set(js.map((j) => j.box_name).filter(Boolean))];
      const where = dir === 'upload' ? `▲ to ${multiBox() && names.length ? names.join(' + ') : 'vault'}` : `▼ to Mac${multiBox() && names.length ? ` from ${names.join(' + ')}` : ''}`;
      return `<section class="batch">
        <div class="batch-head"><h3>${esc(js[0].batch_label)}</h3>
          <span class="meta">${where} · ${js.length} transfer${js.length === 1 ? '' : 's'} · ${bytes(done)} / ${bytes(total)}</span>
          <span class="spacer"></span></div>
        ${js.map(jobHTML).join('')}
      </section>`;
    }).join('');
  }
  $$('[data-tab]', view).forEach((b) => b.addEventListener('click', () => { state.transferTab = b.dataset.tab; renderTransfers(); }));
  $$('[data-job]', view).forEach((b) => b.addEventListener('click', () => {
    const [act, id] = b.dataset.job.split(':');
    const job = jobs.find((j) => j.id === +id);
    if (act === 'reveal') return invoke('reveal', { path: job.local });
    invoke(act, { ids: [+id] });
  }));
}

function jobHTML(j) {
  const pct = j.size ? (j.done / j.size) * 100 : 100;
  let msg = '';
  if (j.state === 'waiting') msg = `<span class="msg wait">${esc(j.error || 'Waiting for the drive')}</span>`;
  else if (j.state === 'retrying') msg = `<span class="msg err">${esc(j.error || 'Connection lost')}${j.retry_in != null ? ` · retrying in ${Math.ceil(j.retry_in)}s` : ''}</span>`;
  else if (j.state === 'failed') msg = `<span class="msg err">${esc(j.error || 'Failed')}</span>`;
  else if (j.state === 'verifying') msg = `<span class="msg">Comparing MD5 with the storage box · ${Math.floor((j.verified / (j.size || 1)) * 100)}%</span>`;
  else if (j.note) msg = `<span class="msg note">${esc(j.note)}</span>`;
  else if (j.state === 'done' && j.offload) msg = `<span class="msg note">Verified · local copy freed</span>`;
  const streams = j.segments?.length > 1 ? `<span>${j.segments.length} streams</span>` : '';
  const stats = j.state === 'running'
    ? `<span class="spd">${speed(j.speed)}</span><span>${pct.toFixed(1)}%</span><span>${humanEta(j.eta)}</span>${streams}`
    : `<span>${bytes(j.done)} / ${bytes(j.size)}</span>${streams}`;
  const acts = [
    ['running', 'verifying', 'queued', 'retrying', 'waiting'].includes(j.state) ? `<button data-job="pause:${j.id}" title="Pause">${icon.pause}</button>` : '',
    ['paused', 'failed', 'canceled'].includes(j.state) ? `<button data-job="resume:${j.id}" title="${j.state === 'paused' ? 'Resume' : 'Retry'}">${j.state === 'paused' ? icon.play : icon.retry}</button>` : '',
    !['done', 'canceled', 'failed'].includes(j.state) ? `<button data-job="cancel:${j.id}" title="Cancel">${icon.x}</button>` : '',
    `<button data-job="reveal:${j.id}" title="Show in Finder">${icon.reveal}</button>`,
  ].join('');
  return `<div class="job ${j.direction} ${j.state}">
    <span class="dir-badge">${j.direction === 'upload' ? icon.up : icon.down}</span>
    <div class="top"><span class="nm">${esc(j.name)}</span>${multiBox() && j.box_name ? `<span class="box-tag">${esc(j.box_name)}</span>` : ''}<span class="path"><bdi>${esc(j.direction === 'upload' ? j.remote : j.local)}</bdi></span><span class="state-tag">${stateLabel[j.state]}</span></div>
    <div class="bottom">${['done', 'canceled'].includes(j.state) ? '' : stripHTML(j)}<span class="stats">${stats}</span>${msg}</div>
    <div class="acts">${acts}</div>
  </div>`;
}

// ───────────────────────── settings view ─────────────────────────

function renderSettings(draft) {
  // `draft` keeps unsaved edits when boxes are added or removed.
  const s = draft || structuredClone(state.settings);
  setHead('<span class="here">preferences</span>', `Settings`, '');
  const sw = (id, on) => `<span class="switch"><input type="checkbox" id="${id}" ${on ? 'checked' : ''}/><span></span></span>`;
  const range = (id, min, max, val, fmt, attrs = '') => `<div class="range-wrap"><input type="range" id="${id}" min="${min}" max="${max}" value="${val}" ${attrs}/><output for="${id}" data-fmt="${fmt}">${val}</output></div>`;
  const boxCard = (b, i) => `<section class="card box-card" data-i="${i}">
      <div class="card-head row-head">
        <div><h3><span class="ico">${icon.vault}</span>${esc(b.name || 'New storage box')}</h3>
          <p>${b.host ? `${esc(b.user)}@${esc(b.host)}` : 'SFTP over SSH, e.g. a Hetzner Storage Box'}</p></div>
        ${s.boxes.length > 1 ? `<button class="btn ghost danger" data-remove-box="${i}" type="button"><span class="ico">${icon.trash}</span>Remove</button>` : ''}
      </div>
      <div class="field"><label>Name<small>Shown in the sidebar and archive dialog</small></label><input class="input" data-f="name" value="${esc(b.name)}" placeholder="e.g. Main, Offsite"/></div>
      <div class="field"><label>Server</label><div class="row"><input class="input" data-f="host" value="${esc(b.host)}" placeholder="u123456.your-storagebox.de" spellcheck="false"/><input class="input port" data-f="port" value="${b.port}" inputmode="numeric"/></div></div>
      <div class="field"><label>Username</label><input class="input" data-f="user" value="${esc(b.user)}" placeholder="u123456" spellcheck="false"/></div>
      <div class="field top"><label>SSH key<small>The box must list its public key in authorized keys</small></label>
        <div>
          <div class="row"><input class="input" data-f="key_path" value="${esc(b.key_path)}" spellcheck="false"/><button class="btn" data-choose-key type="button">Choose…</button></div>
          <div class="key-panel"></div>
        </div></div>
      <div class="field"><label>Vault folder<small>Everything is archived under here</small></label><input class="input" data-f="remote_root" value="${esc(b.remote_root)}" spellcheck="false"/></div>
      <div class="field"><label>Connection budget<small>Max 10 per box · one kept for browsing</small></label>
        <div>${range(`conns-${i}`, 2, 10, b.max_connections, 'conns', 'data-f="max_connections"')}<div class="budget-viz"></div></div></div>
      <div class="field"><label></label><div class="row"><button class="btn" data-test type="button">Test connection</button><span class="test-result"></span></div></div>
      ${state.settings.boxes.some((x) => x.id && x.id === b.id) ? `<div class="field top"><label>Who can sign in<small>Add a teammate’s public key so their Klone can use this box</small></label>
        <div class="authz"><button class="btn" data-authz-load type="button"><span class="ico">${icon.key}</span>Manage access keys</button></div></div>` : ''}
    </section>`;

  $('#view').innerHTML = `<form class="settings stagger" id="settings-form" onsubmit="return false">
    <div class="settings-section-head"><h4>Storage boxes</h4><span>Like drives: each archive can go to one or several.</span></div>
    ${s.boxes.map(boxCard).join('')}
    <button class="nav-add add-box" id="add-box" type="button"><span class="ico">${icon.plus}</span>Add storage box</button>

    <section class="card">
      <div class="card-head"><h3>Throughput</h3><p>Applies to every storage box. Each box also has its own connection budget above.</p></div>
      <div class="field"><label>Files at once<small>Per storage box</small></label>${range('concurrent_files', 1, 6, s.concurrent_files, 'files')}</div>
      <div class="field"><label>Streams per large file<small>Parallel connections for one file</small></label>${range('streams_per_file', 1, 8, s.streams_per_file, 'streams')}</div>
      <div class="field"><label>Split files larger than</label>${range('segment_threshold_mb', 16, 2048, s.segment_threshold_mb, 'mb')}</div>
      <div class="field"><label>Bandwidth limit<small>Shared by all boxes · 0 = use everything</small></label>${range('bandwidth_limit_mbps', 0, 1000, s.bandwidth_limit_mbps, 'mbps')}</div>
    </section>

    <section class="card">
      <div class="card-head"><h3>Safety</h3><p>How careful Klone is with your footage.</p></div>
      <div class="field"><label>Verify checksums<small>MD5 on both ends after each transfer. Always on when freeing space.</small></label>${sw('verify_checksums', s.verify_checksums)}</div>
      <div class="field"><label>When freeing space</label><div class="segmented" id="offload_mode">
        <button type="button" data-v="trash" class="${s.offload_mode === 'trash' ? 'on' : ''}">Move to Trash</button>
        <button type="button" data-v="delete" class="${s.offload_mode === 'delete' ? 'on' : ''}">Delete immediately</button></div></div>
      <div class="field"><label>Keep Mac awake<small>While transfers are running</small></label>${sw('prevent_sleep', s.prevent_sleep)}</div>
      <div class="field"><label>Notifications<small>When a batch finishes</small></label>${sw('notifications', s.notifications)}</div>
      <div class="field"><label>Skip hidden files<small>.DS_Store, ._ resource forks…</small></label>${sw('skip_hidden', s.skip_hidden)}</div>
    </section>

    <section class="card">
      <div class="card-head"><h3>About</h3><p>Klone ${esc(state.version || '')}</p></div>
      <div class="field"><label>Updates<small>Checked every few hours from GitHub</small></label>
        <div class="row"><button class="btn" id="check-update" type="button"><span class="ico">${icon.refresh}</span>Check for updates</button><span class="test-result" id="update-result"></span></div></div>
    </section>

    <div class="settings-foot"><button class="btn primary" id="save-settings" type="button">Save settings</button></div>
  </form>`;

  const fmt = { conns: (v) => `${v} conns`, files: (v) => `${v} file${v == 1 ? '' : 's'}`, streams: (v) => `${v} stream${v == 1 ? '' : 's'}`, mb: (v) => v >= 1024 ? `${(v / 1024).toFixed(1)} GB` : `${v} MB`, mbps: (v) => (+v ? `${v} Mbit/s` : 'unlimited') };
  const cards = () => $$('#settings-form .box-card');
  const sync = () => {
    $$('#settings-form output').forEach((o) => { o.textContent = fmt[o.dataset.fmt]($('#' + o.htmlFor).value); });
    let minCap = 9;
    cards().forEach((c) => {
      const max = +$('[data-f="max_connections"]', c).value;
      minCap = Math.min(minCap, max - 1);
      $('.budget-viz', c).innerHTML = Array.from({ length: 10 }, (_, i) => `<span class="lamp ${i < max ? (i === max - 1 ? 'on browse' : 'on') : 'reserved'}"></span>`).join('');
    });
    $('#concurrent_files').max = Math.max(1, Math.min(6, minCap));
  };
  $$('#settings-form input[type=range]').forEach((r) => r.addEventListener('input', sync));
  sync();
  $$('#offload_mode button').forEach((b) => b.addEventListener('click', () => {
    $$('#offload_mode button').forEach((x) => x.classList.toggle('on', x === b));
  }));

  const readBox = (c) => {
    const base = s.boxes[+c.dataset.i];
    const f = (k) => $(`[data-f="${k}"]`, c).value.trim();
    return { ...base, name: f('name'), host: f('host'), port: +f('port') || 23, user: f('user'), key_path: f('key_path'),
      remote_root: f('remote_root') || '/home/klone', max_connections: +f('max_connections') };
  };
  const collect = () => ({
    ...s,
    boxes: cards().map(readBox),
    concurrent_files: +$('#concurrent_files').value,
    streams_per_file: +$('#streams_per_file').value, segment_threshold_mb: +$('#segment_threshold_mb').value,
    bandwidth_limit_mbps: +$('#bandwidth_limit_mbps').value,
    verify_checksums: $('#verify_checksums').checked, prevent_sleep: $('#prevent_sleep').checked,
    notifications: $('#notifications').checked, skip_hidden: $('#skip_hidden').checked,
    offload_mode: $('#offload_mode .on').dataset.v,
  });
  const save = async (msg = 'Settings saved') => {
    try {
      state.settings = await invoke('save_settings', { settings: collect() });
      toast(msg);
      refreshUsage();
      renderSettings();
    } catch (e) { toast(String(e), true); }
  };

  $('#add-box').addEventListener('click', () => {
    const d = collect();
    const first = d.boxes[0];
    d.boxes.push({ id: '', name: '', host: '', port: 23, user: '', key_path: first?.key_path || '', remote_root: '/home/klone', max_connections: 8 });
    renderSettings(d);
    const last = cards().at(-1);
    last?.scrollIntoView({ behavior: 'smooth', block: 'start' });
    $('[data-f="name"]', last)?.focus();
  });
  $$('[data-remove-box]').forEach((btn) => btn.addEventListener('click', () => {
    const d = collect();
    const i = +btn.dataset.removeBox;
    const b = d.boxes[i];
    const busy = state.snapshot.jobs.filter((j) => j.box_id === b.id && !['done', 'canceled', 'failed'].includes(j.state)).length;
    modal(`<div class="dialog"><h2>Remove “${esc(b.name || 'this box')}”?</h2>
      <p>Klone will stop using this storage box. Nothing on the box is deleted.${busy ? ` <b>${busy} unfinished transfer${busy === 1 ? '' : 's'}</b> to it will be stopped.` : ''}</p>
      <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button><button class="btn danger solid" data-c="ok">Remove box</button></div></div>`,
    (c) => {
      if (c !== 'ok') return;
      d.boxes.splice(i, 1);
      if (!b.id) return renderSettings(d);
      renderSettings(d);
      save(`Removed ${b.name}`);
    });
  }));

  // ── per-box SSH key panel, test connection ──
  cards().forEach((c) => {
    const keyInput = $('[data-f="key_path"]', c);
    const panel = $('.key-panel', c);
    const keyPanel = async () => {
      const path = keyInput.value.trim();
      if (!path) { panel.innerHTML = ''; return; }
      let k;
      try { k = await invoke('key_info', { path }); } catch { k = { exists: false }; }
      if (state.view !== 'settings' || keyInput.value.trim() !== path) return;
      if (!k.public_key) {
        panel.innerHTML = `<div class="key-empty">
            <span>${k.exists ? 'Couldn’t read a public key for this file (it may be passphrase-protected).' : 'No key at this path yet.'}</span>
            <button class="btn primary" data-gen type="button"><span class="ico">${icon.key}</span>Create a key</button>
          </div>`;
      } else {
        panel.innerHTML = `<div class="key-meta"><span class="key-kind">${esc(k.kind || 'key')}</span><code class="key-fp">${esc(k.fingerprint || '')}</code></div>
          <pre class="key-pub" title="Public key">${esc(k.public_key)}</pre>
          <div class="row key-acts">
            <button class="btn primary" data-copy type="button"><span class="ico">${icon.copy}</span>Copy public key</button>
            <button class="btn ghost" data-gen type="button"><span class="ico">${icon.key}</span>Create new key</button>
          </div>
          <ol class="key-steps">
            <li>Paste the public key into this box’s <code>.ssh/authorized_keys</code>, or send it to whoever manages the box.</li>
            <li>Click <b>Test connection</b> below.</li>
          </ol>`;
        $('[data-copy]', panel).addEventListener('click', async () => {
          try { await invoke('copy_public_key', { path }); toast('Public key copied to the clipboard'); }
          catch (e) { toast(String(e), true); }
        });
      }
      $('[data-gen]', panel).addEventListener('click', () => (k.public_key ? confirmNewKey(k.path) : generateKey()));
    };
    const generateKey = async () => {
      try {
        const k = await invoke('generate_key');
        keyInput.value = k.path;
        await save('New key created — public key copied to the clipboard');
      } catch (e) { toast(String(e), true); }
    };
    const confirmNewKey = (current) => modal(`<div class="dialog">
        <h2>Create a new SSH key?</h2>
        <p>This box will switch to the new key straight away and won’t connect until the new public key is added to it. Your current key (<bdi class="mono">${esc(current)}</bdi>) is left untouched.</p>
        <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button><button class="btn primary" data-c="ok">Create key</button></div>
      </div>`, (r) => r === 'ok' && generateKey());
    let keyTimer;
    keyInput.addEventListener('input', () => { clearTimeout(keyTimer); keyTimer = setTimeout(keyPanel, 350); });
    $('[data-choose-key]', c).addEventListener('click', async () => {
      const cur = keyInput.value.trim();
      const path = await T.dialog.open({ title: 'Choose your SSH private key', defaultPath: cur.slice(0, cur.lastIndexOf('/')) || undefined });
      if (path) { keyInput.value = path.replace(/\.pub$/, ''); keyPanel(); }
    });
    keyPanel();

    $('[data-test]', c).addEventListener('click', async () => {
      const out = $('.test-result', c);
      out.className = 'test-result'; out.textContent = 'Connecting…';
      try {
        const u = await invoke('test_connection', { storageBox: readBox(c) });
        out.className = 'test-result ok'; out.textContent = `Connected · ${ibytes(u.used)} of ${ibytes(u.total, 0)} used`;
      } catch (e) { out.className = 'test-result err'; out.textContent = String(e); }
    });

    // ── who can sign in: the box's authorized_keys ──
    const authz = $('.authz', c);
    if (!authz) return;
    const boxId = s.boxes[+c.dataset.i].id;
    const draw = (r) => {
      authz.innerHTML = `<ul class="authz-list">${r.keys.map((k, i) => `<li class="${k.mine ? 'mine' : ''}">
          <span class="key-kind">${esc(k.kind.replace(/^ssh-|^ecdsa-sha2-|@openssh\.com$/g, ''))}</span>
          <div class="authz-who"><b>${esc(k.comment || 'Unnamed key')}</b><code>${esc(k.fingerprint)}</code></div>
          ${k.mine ? '<span class="authz-tag">This Mac</span>' : `<button class="btn ghost danger icon-only" data-revoke="${i}" type="button" title="Remove access">${icon.trash}</button>`}
        </li>`).join('') || '<li class="authz-none">No keys listed — this box may use a password.</li>'}</ul>
        <textarea class="input authz-paste" rows="2" spellcheck="false" placeholder="Paste a public key — ssh-ed25519 AAAA… name@their-mac"></textarea>
        <div class="row key-acts"><button class="btn primary" data-authz-add type="button"><span class="ico">${icon.plus}</span>Add key</button>
          <button class="btn ghost" data-authz-file type="button">From .pub file…</button>
          <span class="authz-path" title="Stored on the box">${esc(r.path)}</span></div>
        <ol class="key-steps"><li>Ask them to open Klone → Settings → <b>Copy public key</b> (or send their <code>.pub</code> file).</li>
          <li>Add it here. They use this box’s server and username with their own key — never share your private key.</li></ol>`;
      const paste = $('.authz-paste', authz);
      const add = async (key) => {
        if (!key.trim()) { paste.focus(); return; }
        try {
          const r2 = await invoke('add_authorized_key', { boxId, key });
          draw(r2);
          toast(r2.changed ? 'Key added — they can sign in now' : 'That key already has access');
        } catch (e) { toast(String(e), true); }
      };
      $('[data-authz-add]', authz).addEventListener('click', () => add(paste.value));
      $('[data-authz-file]', authz).addEventListener('click', async () => {
        const path = await T.dialog.open({ title: 'Choose a public key (.pub)', filters: [{ name: 'Public key', extensions: ['pub'] }] });
        if (!path) return;
        try {
          const k = await invoke('key_info', { path });
          if (!k.public_key) throw new Error('No public key in that file');
          add(k.public_key);
        } catch (e) { toast(String(e), true); }
      });
      $$('[data-revoke]', authz).forEach((btn) => btn.addEventListener('click', () => {
        const k = r.keys[+btn.dataset.revoke];
        modal(`<div class="dialog"><h2>Remove access for “${esc(k.comment || 'this key')}”?</h2>
          <p>Anyone using this key (<span class="mono">${esc(k.fingerprint)}</span>) won’t be able to sign in to ${esc(s.boxes[+c.dataset.i].name)} any more. Files on the box are not affected. A backup of the old list is kept next to it on the box.</p>
          <div class="actions"><button class="btn ghost" data-c="cancel">Cancel</button><button class="btn danger solid" data-c="ok">Remove access</button></div></div>`,
        async (choice) => {
          if (choice !== 'ok') return;
          try { draw(await invoke('remove_authorized_key', { boxId, id: k.id })); toast('Access removed'); }
          catch (e) { toast(String(e), true); }
        });
      }));
    };
    $('[data-authz-load]', authz).addEventListener('click', async (ev) => {
      const btn = ev.currentTarget;
      btn.disabled = true;
      btn.lastChild.textContent = 'Loading…';
      try { draw(await invoke('authorized_keys', { boxId })); }
      catch (e) { authz.innerHTML = `<div class="key-empty"><span>${esc(String(e))}</span></div>`; }
    });
  });

  $('#save-settings').addEventListener('click', () => save());
  $('#check-update').addEventListener('click', async () => {
    const out = $('#update-result');
    out.className = 'test-result'; out.textContent = 'Checking…';
    try {
      const u = await invoke('check_update');
      if (!u) { out.className = 'test-result ok'; out.textContent = `You’re up to date (${state.version})`; return; }
      out.textContent = `${u.version} is available`;
      showUpdate(u);
      offerUpdate(u);
    } catch (e) { out.className = 'test-result err'; out.textContent = `Couldn’t check: ${e}`; }
  });
}

// ───────────────────────── live updates ─────────────────────────

let lastFinished = null;
let renderQueued = false;
let refreshTimer = null;

/** Refresh listings after files finish, at most every few seconds. */
function scheduleRefresh() {
  if (refreshTimer) return;
  refreshTimer = setTimeout(() => {
    refreshTimer = null;
    reload(); refreshUsage(); refreshLibraries();
  }, 2500);
}

function onSnapshot(snap) {
  const prev = state.snapshot;
  state.snapshot = snap;
  const s = snap.stats;

  // transport readouts
  const [un, uu] = speedParts(s.up_speed);
  const [dn, du] = speedParts(s.down_speed);
  $('#up-num').textContent = un; $('#up-unit').textContent = uu;
  $('#down-num').textContent = dn; $('#down-unit').textContent = du;
  $('.readout.up').classList.toggle('live', s.up_speed > 1000);
  $('.readout.down').classList.toggle('live', s.down_speed > 1000);
  $('#eta').textContent = s.active ? timecode(s.eta) : '--:--:--';
  $('#queue-label').textContent = s.active || s.queued
    ? `${s.active} moving · ${s.queued} queued`
    : s.waiting ? `${s.waiting} waiting for ${(s.offline_drives || []).join(', ') || 'drive'}`
    : s.paused ? `${s.paused} paused` : s.failed ? `${s.failed} need attention` : 'idle · all safe';
  $('#remaining').textContent = s.remaining_bytes ? `${bytes(s.remaining_bytes)} to go` : (s.session_bytes ? `${bytes(s.session_bytes)} moved this session` : '—');
  const groups = s.boxes?.length ? s.boxes : [{ name: 'Storage box', connections: s.connections || 0, max_connections: s.max_connections || 8 }];
  $('#lamps').innerHTML = groups.map((g) => {
    const slots = groups.length > 1 ? g.max_connections : 10;
    return `<span class="lamp-group" title="${esc(g.name)}: ${g.connections} of ${g.max_connections} SSH connections in use (limit 10 per box)">${Array.from({ length: slots }, (_, i) =>
      `<span class="lamp ${i >= g.max_connections ? 'reserved' : ''} ${i === g.max_connections - 1 ? 'browse' : ''} ${i < g.connections ? 'on' : ''}"></span>`).join('')}</span>`;
  }).join('');
  const busy = s.active + s.queued > 0;
  $('#toggle-all').innerHTML = busy ? icon.pause : icon.play;
  $('#toggle-all').title = busy ? 'Pause all transfers' : 'Resume all transfers';
  document.body.classList.toggle('busy', s.active > 0);
  const count = s.active + s.queued + s.failed;
  const badge = $('#transfer-count');
  badge.hidden = !count;
  badge.textContent = count;
  drawScope($('#scope'), s.history);

  // When files finish, refresh listings so badges flip to "in vault".
  // Use the engine's total: the job list is truncated, so counting it
  // would change every tick once many jobs finish or fail.
  const finished = s.done_total ?? 0;
  if (finished !== lastFinished) {
    const first = lastFinished === null;
    lastFinished = finished;
    if (!first && prev.jobs.length) scheduleRefresh();
  }

  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    if (state.view === 'transfers' && !document.querySelector('.modal:not([hidden])')) renderTransfers();
    else if (state.view === 'library' || state.view === 'vault') patchMovingPills();
  });
}

/** Update in-flight percentages without re-rendering the whole table. */
function patchMovingPills() {
  for (const r of state.rows) {
    if (!r.jobs?.length) continue;
    const tr = document.querySelector(`tr[data-key="${CSS.escape(r.key)}"]`);
    if (!tr) continue;
    const job = activeJob(r);
    if (job) tr.children[3].innerHTML = movingPill(job);
  }
}

// ───────────────────────── drag & drop ─────────────────────────

function setupDrop() {
  const veil = $('#drop-veil');
  T.webviewWindow.getCurrentWebviewWindow().onDragDropEvent((e) => {
    const p = e.payload;
    if (p.type === 'enter' || p.type === 'over') {
      if (veil.hidden) {
        const lib = state.libraries.find((l) => p.paths?.[0]?.startsWith(l.path + '/'));
        $('#drop-hint').textContent = lib ? `Mirrored into ${lib.name}/, keeping folder structure.` : 'Files outside a library go to Unsorted/.';
      }
      veil.hidden = false;
    } else if (p.type === 'leave') {
      veil.hidden = true;
    } else if (p.type === 'drop') {
      veil.hidden = true;
      if (p.paths?.length) archiveFlow(p.paths, [], 'ask', p.paths.length === 1 ? basename(p.paths[0]) : `${p.paths.length} items`);
    }
  });
}

// ───────────────────────── keyboard ─────────────────────────

function onKey(e) {
  if (e.target.matches('input, textarea')) {
    if (e.key === 'Escape') e.target.blur();
    return;
  }
  if (!$('#modal').hidden) {
    if (e.key === 'Escape') closeModal();
    return;
  }
  if (e.key === 'Escape') { state.selection.clear(); render(); }
  if (e.metaKey && e.key === 'a' && ['library', 'vault'].includes(state.view)) {
    e.preventDefault();
    state.rows.forEach((r) => state.selection.add(r.key));
    render();
  }
  if (e.metaKey && e.key === 'ArrowUp') { e.preventDefault(); goUp(); }
  if (e.metaKey && e.key === 'r') { e.preventDefault(); reload(); }
  if (e.key === 'Enter' && state.selection.size === 1) openRow(selectedRows()[0]);
  if (e.metaKey && e.key === ',') { e.preventDefault(); go('settings'); }
}

// ───────────────────────── chrome helpers ─────────────────────────

function setHead(crumbs, title, toolbar) {
  $('#crumbs').innerHTML = crumbs;
  $('#title').innerHTML = title;
  $('#toolbar').innerHTML = toolbar;
}

let modalHandler = null;
function modal(html, onChoice, onOpen) {
  const m = $('#modal');
  m.innerHTML = html;
  m.hidden = false;
  modalHandler = onChoice;
  $$('[data-c]', m).forEach((b) => b.addEventListener('click', () => {
    const c = b.dataset.c;
    // Read the form before closeModal() empties the dialog.
    const form = Object.fromEntries($$('input[id], select[id], textarea[id]', m)
      .map((i) => [i.id, i.type === 'checkbox' ? i.checked : i.value]));
    closeModal();
    onChoice?.(c, form);
  }));
  m.onclick = (e) => { if (e.target === m) closeModal(); };
  m.onkeydown = (e) => { if (e.key === 'Enter' && e.target.matches('input')) $('[data-c="ok"]', m)?.click(); };
  onOpen?.();
}
function closeModal() {
  const m = $('#modal');
  m.hidden = true;
  m.innerHTML = '';
  modalHandler = null;
}

function toast(msg, err = false) {
  const t = document.createElement('div');
  t.className = 'toast' + (err ? ' err' : '');
  t.textContent = msg;
  $('#toasts').appendChild(t);
  setTimeout(() => { t.classList.add('leaving'); setTimeout(() => t.remove(), 260); }, err ? 7000 : 3400);
}

function hydrateIcons(root) {
  $$('[data-icon]', root).forEach((el) => { el.innerHTML = icon[el.dataset.icon] || ''; });
}

function relPath(base, p) {
  if (!p || p === base) return '';
  return p.startsWith(base + '/') ? p.slice(base.length + 1) : p;
}
function joinLocal(dir, name) { return dir.endsWith('/') ? dir + name : `${dir}/${name}`; }
function joinRemote(dir, name) { return dir.endsWith('/') ? dir + name : `${dir}/${name}`; }
function basename(p) { return p.replace(/\/+$/, '').split('/').pop(); }
function driveName(p) { const m = /^\/Volumes\/([^/]+)/.exec(p || ''); return m ? m[1] : ''; }
