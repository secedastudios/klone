import { invoke, listen, $, esc, bytes, speedParts, timecode, humanEta, icon, drawScope, stripHTML, stateLabel } from './common.js';

let snap = await invoke('snapshot');
render();
listen('snapshot', ({ payload }) => { snap = payload; render(); });

$('#p-open').addEventListener('click', () => invoke('show_main'));
$('#p-quit').innerHTML = icon.power;
$('#p-quit').addEventListener('click', () => invoke('quit'));
$('#p-toggle').addEventListener('click', () => {
  const s = snap.stats;
  invoke(s.active + s.queued > 0 ? 'pause_all' : 'resume_all');
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') window.__TAURI__.window.getCurrentWindow().hide();
});

function render() {
  const s = snap.stats || {};
  const [u, uu] = speedParts(s.up_speed);
  const [d, du] = speedParts(s.down_speed);
  $('#p-up').textContent = u; $('#p-up-u').textContent = uu;
  $('#p-down').textContent = d; $('#p-down-u').textContent = du;
  document.querySelector('.readout.up').classList.toggle('live', s.up_speed > 1000);
  document.querySelector('.readout.down').classList.toggle('live', s.down_speed > 1000);
  document.body.classList.toggle('busy', s.active > 0);
  $('#p-state').textContent = s.active ? `${s.active} moving` : s.queued ? 'queued' : s.waiting ? 'waiting for drive' : s.paused ? 'paused' : 'idle';
  $('#p-eta').textContent = s.active ? timecode(s.eta) : '--:--:--';
  $('#p-left').innerHTML = s.remaining_bytes ? `<b>${bytes(s.remaining_bytes)}</b> to go · ${s.queued} queued` : `${s.connections || 0}/${s.max_connections || 8} connections`;
  const busy = s.active + s.queued > 0;
  $('#p-toggle').innerHTML = busy ? icon.pause : icon.play;
  $('#p-toggle').title = busy ? 'Pause all' : 'Resume all';
  drawScope($('#p-scope'), s.history);

  const live = (snap.jobs || []).filter((j) => ['running', 'verifying', 'retrying', 'paused', 'waiting', 'failed'].includes(j.state));
  const list = $('#p-list');
  if (!live.length) {
    const recent = (snap.jobs || []).filter((j) => j.state === 'done').length;
    list.innerHTML = `<div class="pop-idle"><div><h3>All quiet</h3><p>${recent ? `${recent} transfer${recent === 1 ? '' : 's'} completed recently.` : 'Drop files on the Klone window to archive them.'}</p></div></div>`;
    return;
  }
  list.innerHTML = live.slice(0, 8).map((j) => {
    const pct = j.size ? Math.floor((j.done / j.size) * 100) : 0;
    const right = j.state === 'running' ? humanEta(j.eta) : stateLabel[j.state];
    const left = j.state === 'retrying' || j.state === 'failed'
      ? `<span class="err">${esc(j.error || '')}</span>`
      : `<span>${bytes(j.done)} / ${bytes(j.size)}</span>`;
    return `<div class="pop-job ${j.direction}">
      <div class="l1"><span class="ico">${j.direction === 'upload' ? icon.up : icon.down}</span><span class="nm">${esc(j.name)}</span><span class="pct">${pct}%</span></div>
      ${stripHTML(j)}
      <div class="l3">${left}<span>${right}</span></div>
    </div>`;
  }).join('');
}
