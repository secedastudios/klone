// Shared helpers for the main window and the menu bar popover.

export const T = window.__TAURI__;
export const invoke = (cmd, args) => T.core.invoke(cmd, args);
export const listen = (ev, fn) => T.event.listen(ev, fn);

export const $ = (sel, root = document) => root.querySelector(sel);
export const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

export function esc(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
}

/** Decimal units, like Finder. */
export function bytes(n, digits = 1) {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let i = 0;
  while (n >= 1000 && i < u.length - 1) { n /= 1000; i++; }
  return i === 0 ? `${n} B` : `${n.toFixed(n >= 100 ? 0 : digits)} ${u[i]}`;
}

/** Binary units labelled the way Hetzner reports them (10 TB box). */
export function ibytes(n, digits = 1) {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB', 'TB', 'PB'];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return i === 0 ? `${n} B` : `${n.toFixed(n >= 100 || digits === 0 ? 0 : digits)} ${u[i]}`;
}

export function speed(bps) {
  if (!bps || bps < 1000) return '0';
  return bytes(bps) + '/s';
}

/** Split a speed into number + unit so the number can be set large. */
export function speedParts(bps) {
  if (!bps || bps < 1000) return ['0.0', 'MB/s'];
  const mb = bps / 1e6;
  if (mb >= 1000) return [(mb / 1000).toFixed(2), 'GB/s'];
  if (mb >= 1) return [mb.toFixed(mb >= 100 ? 0 : 1), 'MB/s'];
  return [(bps / 1000).toFixed(0), 'KB/s'];
}

/** ETA as broadcast timecode HH:MM:SS. */
export function timecode(sec) {
  if (sec == null || !isFinite(sec)) return '--:--:--';
  sec = Math.max(0, Math.round(sec));
  const h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = sec % 60;
  return [h, m, s].map((v) => String(v).padStart(2, '0')).join(':');
}

export function humanEta(sec) {
  if (sec == null || !isFinite(sec)) return '';
  if (sec < 60) return `${Math.max(1, Math.round(sec))}s left`;
  if (sec < 3600) return `${Math.round(sec / 60)} min left`;
  const h = Math.floor(sec / 3600), m = Math.round((sec % 3600) / 60);
  return `${h}h ${m}m left`;
}

export function date(secs) {
  if (!secs) return '—';
  const d = new Date(secs * 1000);
  const now = new Date();
  const sameYear = d.getFullYear() === now.getFullYear();
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric', ...(sameYear ? {} : { year: 'numeric' }) }) +
    (sameYear ? ' · ' + d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' }) : '');
}

const VIDEO = /\.(mov|mp4|m4v|mxf|r3d|braw|ari|arx|crm|avi|mkv|mts|m2ts|prores|dng|webm|cine)$/i;
const AUDIO = /\.(wav|aif|aiff|mp3|m4a|flac|bwf|aac|ogg)$/i;
const IMAGE = /\.(jpe?g|png|tiff?|heic|psd|exr|dpx|cr2|cr3|nef|arw|raf|gif|webp|svg)$/i;
const PROJECT = /\.(drp|drt|prproj|fcpbundle|aep|veg|edl|xml|fcpxml|aaf|omf|otio)$/i;

export function kind(name, isDir) {
  if (isDir) return 'folder';
  if (VIDEO.test(name)) return 'video';
  if (AUDIO.test(name)) return 'audio';
  if (IMAGE.test(name)) return 'image';
  if (PROJECT.test(name)) return 'project';
  return 'file';
}

const P = (d, extra = '') => `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true" ${extra}>${d}</svg>`;

export const icon = {
  key: P('<circle cx="8" cy="15" r="4"/><path d="M11 12 20 3M16.5 6.5l2.5 2.5M14 9l2 2"/>'),
  copy: P('<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V5.5A1.5 1.5 0 0 0 14.5 4h-9A1.5 1.5 0 0 0 4 5.5v9A1.5 1.5 0 0 0 5.5 16H8"/>'),
  folder: P('<path d="M3 7.5A1.5 1.5 0 0 1 4.5 6h4.2l2 2h8.8A1.5 1.5 0 0 1 21 9.5v8A1.5 1.5 0 0 1 19.5 19h-15A1.5 1.5 0 0 1 3 17.5z"/>'),
  video: P('<rect x="3" y="5" width="18" height="14" rx="1.5"/><path d="M7 5v14M17 5v14M3 9h4M3 15h4M17 9h4M17 15h4"/>'),
  audio: P('<path d="M4 10v4M8 7v10M12 4v16M16 8v8M20 11v2"/>'),
  image: P('<rect x="3" y="5" width="18" height="14" rx="1.5"/><circle cx="9" cy="10" r="1.6"/><path d="m21 16-5-5-8 8"/>'),
  project: P('<path d="M4 4h10l6 6v10H4z"/><path d="M14 4v6h6M8 15h8M8 18h5"/>'),
  file: P('<path d="M6 3h8l5 5v13H6z"/><path d="M14 3v5h5"/>'),
  up: P('<path d="M12 19V5M6 11l6-6 6 6"/>'),
  down: P('<path d="M12 5v14M6 13l6 6 6-6"/>'),
  pause: P('<path d="M9 5v14M15 5v14"/>', 'stroke-width="2.2"'),
  play: P('<path d="M7 5l12 7-12 7z"/>'),
  x: P('<path d="M6 6l12 12M18 6 6 18"/>'),
  retry: P('<path d="M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7"/>'),
  reveal: P('<circle cx="11" cy="11" r="6"/><path d="m20 20-4.5-4.5"/>'),
  plus: P('<path d="M12 5v14M5 12h14"/>'),
  gear: P('<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>'),
  check: P('<path d="m5 12.5 4.5 4.5L19 7.5"/>'),
  vault: P('<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="12" cy="12" r="4"/><path d="M12 8v1.5M12 14.5V16M8 12h1.5M14.5 12H16"/>'),
  drive: P('<rect x="3" y="13" width="18" height="7" rx="1.5"/><path d="M5 13l2.5-8h9L19 13M7 16.5h.01M10 16.5h.01"/>'),
  stack: P('<path d="m12 3 9 5-9 5-9-5z"/><path d="m3 13 9 5 9-5"/>'),
  trash: P('<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>'),
  chev: P('<path d="m9 6 6 6-6 6"/>'),
  back: P('<path d="m15 6-6 6 6 6"/>'),
  refresh: P('<path d="M20 11a8 8 0 1 0-2.3 5.7M20 4v7h-7"/>'),
  open: P('<path d="M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5"/>'),
  power: P('<path d="M12 3v9M6.3 6.3a8 8 0 1 0 11.4 0"/>'),
  free: P('<path d="M4 7h16M6 7l1 13h10l1-13"/><path d="M12 17v-6M9.5 13.5 12 11l2.5 2.5"/>'),
};

/** Scope-style dual trace of upload (accent) and download (blue) speed. */
export function drawScope(canvas, history) {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth, h = canvas.clientHeight;
  if (!w || !h) return;
  if (canvas.width !== w * dpr || canvas.height !== h * dpr) {
    canvas.width = w * dpr;
    canvas.height = h * dpr;
  }
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  const css = getComputedStyle(document.documentElement);
  const rgbVar = (n) => css.getPropertyValue(n).trim().replace(/\s+/g, '');

  // graticule
  ctx.strokeStyle = `rgba(${rgbVar('--fg-rgb')},0.06)`;
  ctx.lineWidth = 1;
  for (let i = 1; i < 4; i++) {
    const y = Math.round((h / 4) * i) + 0.5;
    ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(w, y); ctx.stroke();
  }
  for (let i = 1; i < 12; i++) {
    const x = Math.round((w / 12) * i) + 0.5;
    ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, h); ctx.stroke();
  }
  if (!history?.length) return;
  const max = Math.max(1e6, ...history.map((s) => Math.max(s.up, s.down))) * 1.15;
  const trace = (key, rgb) => {
    const pts = history.map((s, i) => [(i / (history.length - 1)) * w, h - (s[key] / max) * (h - 4) - 2]);
    const grad = ctx.createLinearGradient(0, 0, 0, h);
    grad.addColorStop(0, `rgba(${rgb},0.28)`);
    grad.addColorStop(1, `rgba(${rgb},0)`);
    ctx.beginPath();
    ctx.moveTo(0, h);
    pts.forEach(([x, y]) => ctx.lineTo(x, y));
    ctx.lineTo(w, h);
    ctx.closePath();
    ctx.fillStyle = grad;
    ctx.fill();
    ctx.beginPath();
    pts.forEach(([x, y], i) => (i ? ctx.lineTo(x, y) : ctx.moveTo(x, y)));
    ctx.strokeStyle = `rgb(${rgb})`;
    ctx.lineWidth = 1.5;
    ctx.shadowColor = `rgba(${rgb},0.8)`;
    ctx.shadowBlur = 6;
    ctx.stroke();
    ctx.shadowBlur = 0;
  };
  if (history.some((s) => s.down > 0)) trace('down', rgbVar('--down-rgb'));
  trace('up', rgbVar('--up-rgb'));
}

/** Film-strip progress: one lane per parallel stream. */
export function stripHTML(job) {
  const size = job.size || 1;
  const segs = job.segments?.length ? job.segments : [{ start: 0, end: size, done: job.done }];
  if (job.state === 'done') return `<div class="strip done"><i style="--w:100%"></i></div>`;
  const lanes = segs
    .map((s) => {
      const len = Math.max(1, s.end - s.start);
      const left = (s.start / size) * 100;
      const width = (len / size) * 100;
      const fill = Math.min(100, (s.done / len) * 100);
      return `<span class="lane" style="left:${left}%;width:${width}%"><i style="--w:${fill}%"></i></span>`;
    })
    .join('');
  const verify = job.state === 'verifying' ? `<b class="verify" style="--w:${Math.min(100, (job.verified / size) * 100)}%"></b>` : '';
  return `<div class="strip ${job.direction} ${job.state}">${lanes}${verify}</div>`;
}

export const stateLabel = {
  queued: 'Queued',
  running: 'Transferring',
  verifying: 'Verifying',
  paused: 'Paused',
  waiting: 'Waiting for drive',
  retrying: 'Reconnecting',
  done: 'Done',
  failed: 'Failed',
  canceled: 'Canceled',
};

/** Klone logo mark (chevron over stacked bars). */
export const markSVG = (cls = '') => '<svg class="mark" viewBox="0 0 24 24" aria-hidden="true"><path class="m3" d="M8 8.6 12 5l4 3.6"/><rect class="m2" x="6.5" y="11.6" width="11" height="3.4" rx="1.4"/><rect class="m1" x="3.5" y="17.4" width="17" height="3.4" rx="1.4"/></svg>'.replace('class="mark"', `class="mark ${cls}"`);
