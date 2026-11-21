// Shared helpers: API client, event stream, DOM builder, JSON rendering, diffing.

// Let views write `cond ? node : null` inline: skip null/undefined/false children.
for (const proto of [Element.prototype, DocumentFragment.prototype]) {
  for (const method of ['append', 'prepend']) {
    const native = proto[method];
    proto[method] = function (...nodes) { return native.apply(this, nodes.filter((n) => n !== null && n !== undefined && n !== false)); };
  }
}

const TOKEN_KEY = 'branchmesh.token';
export function getToken() { try { return localStorage.getItem(TOKEN_KEY) || ''; } catch { return ''; } }
function setToken(t) { try { localStorage.setItem(TOKEN_KEY, t); } catch { /* ignore */ } }

export function setStoredToken(t) { setToken(t); }

export async function api(path, { method = 'GET', body, quiet = false, token: explicit, raw, contentType } = {}) {
  const headers = { 'content-type': contentType || 'application/json' };
  const token = explicit ?? getToken();
  if (token) headers.authorization = `Bearer ${token}`;
  const payload = raw !== undefined ? raw : (body === undefined ? undefined : JSON.stringify(body));
  const res = await fetch(`/api${path}`, { method, headers, body: payload });
  if (res.status === 401 && explicit === undefined) {
    const t = prompt('This BranchMesh server requires an API token (BRANCHMESH_API_TOKEN):');
    if (t) { setToken(t.trim()); return api(path, { method, body, quiet, raw, contentType }); }
  }
  let data = null;
  const text = await res.text();
  try { data = text ? JSON.parse(text) : null; } catch { data = { error: text }; }
  if (!res.ok) {
    const err = new Error((data && data.error) || `${res.status} ${res.statusText}`);
    err.status = res.status; err.data = data;
    if (!quiet) toast(err.message, 'error');
    throw err;
  }
  return data;
}

export const q = (params) => '?' + new URLSearchParams(Object.entries(params).filter(([, v]) => v !== undefined && v !== null && v !== '')).toString();

// ------------------------------------------------------------------ events

const listeners = new Set();
let source = null;
export function onEvent(fn) { listeners.add(fn); return () => listeners.delete(fn); }

export function connectEvents() {
  const token = getToken();
  const url = '/api/events' + (token ? `?token=${encodeURIComponent(token)}` : '');
  const conn = document.getElementById('conn');
  const kinds = ['commit', 'branch_created', 'branch_updated', 'branch_deleted', 'merge', 'rollback', 'journal',
    'journal_replayed', 'execution_started', 'execution_finished', 'actor_activated', 'actor_hibernated',
    'mailbox_enqueued', 'mailbox_updated', 'llm_delta', 'persona_saved', 'persona_deleted', 'consultation_started',
    'consultation_finished', 'merge_request', 'recovered'];
  source = new EventSource(url);
  source.onopen = () => { conn.classList.add('live'); conn.querySelector('.label').textContent = 'live'; };
  source.onerror = () => { conn.classList.remove('live'); conn.querySelector('.label').textContent = 'reconnecting'; };
  for (const k of kinds) {
    source.addEventListener(k, (e) => {
      let ev; try { ev = JSON.parse(e.data); } catch { return; }
      for (const fn of listeners) { try { fn(ev); } catch (err) { console.error(err); } }
    });
  }
}

// ------------------------------------------------------------------ DOM

export function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === undefined || v === null || v === false) continue;
    if (k === 'class') el.className = v;
    else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
    else if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2), v);
    else if (k === 'html') el.innerHTML = v;
    else if (v === true) el.setAttribute(k, '');
    else el.setAttribute(k, v);
  }
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

export function clear(el) { while (el.firstChild) el.removeChild(el.firstChild); return el; }

export function toast(msg, kind = 'info') {
  const el = h('div', { class: `toast ${kind}` }, msg);
  document.getElementById('toasts').append(el);
  setTimeout(() => el.classList.add('fade'), 3500);
  setTimeout(() => el.remove(), 4200);
}

export function debounce(fn, ms) {
  let t = null;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
}

export const short = (hash) => (hash || '').slice(0, 10);

export function ago(ms) {
  if (!ms) return '—';
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return new Date(ms).toLocaleString();
}

export const fmtTime = (ms) => ms ? new Date(ms).toLocaleTimeString() : '—';

export function badge(text, kind = '') { return h('span', { class: `badge ${kind}` }, text); }

export function statusOf(branch) { return branch.status?.state || 'active'; }

/** Stable color per string. */
export function colorFor(s) {
  let hsh = 0;
  for (const c of String(s)) hsh = (hsh * 31 + c.charCodeAt(0)) >>> 0;
  return `hsl(${hsh % 360} 70% 62%)`;
}

// ------------------------------------------------------------------ JSON

function esc(s) { return String(s).replace(/[&<>]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c])); }

export function highlightJson(text) {
  return esc(text).replace(/("(?:\\.|[^"\\])*")(\s*:)?|\b(true|false|null)\b|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)/g,
    (m, str, colon, lit, num) => {
      if (str) return colon ? `<span class="j-key">${str}</span>${colon}` : `<span class="j-str">${str}</span>`;
      if (lit) return `<span class="j-lit">${lit}</span>`;
      return `<span class="j-num">${num}</span>`;
    });
}

export function jsonView(value, { maxHeight } = {}) {
  const text = value === undefined ? 'undefined' : JSON.stringify(value, null, 2);
  const pre = h('pre', { class: 'json', html: highlightJson(text ?? 'null') });
  if (maxHeight) pre.style.maxHeight = maxHeight;
  return pre;
}

/** Collapsible JSON tree. */
export function jsonTree(value, key = null, depth = 0) {
  const isObj = value && typeof value === 'object';
  if (!isObj) {
    return h('div', { class: 'jt-row' }, key !== null ? h('span', { class: 'j-key' }, `${key}: `) : null,
      h('span', { html: highlightJson(JSON.stringify(value)) }));
  }
  const entries = Array.isArray(value) ? value.map((v, i) => [i, v]) : Object.entries(value);
  const open = depth < 2;
  const label = Array.isArray(value) ? `[${entries.length}]` : `{${entries.length}}`;
  const details = h('details', { class: 'jt', open },
    h('summary', {}, key !== null ? h('span', { class: 'j-key' }, `${key} `) : null, h('span', { class: 'muted' }, label)));
  details.addEventListener('toggle', () => {
    if (details.open && !details.dataset.filled) fill();
  }, { once: false });
  const fill = () => {
    details.dataset.filled = '1';
    for (const [k, v] of entries) details.append(jsonTree(v, k, depth + 1));
  };
  if (open) fill();
  return details;
}

// ------------------------------------------------------------------ diff

/** Line-level LCS diff -> [{type: 'same'|'del'|'add', text}] */
export function lineDiff(a, b) {
  const A = a.split('\n'), B = b.split('\n');
  const n = A.length, m = B.length;
  if (n * m > 4_000_000) return [...A.map((t) => ({ type: 'del', text: t })), ...B.map((t) => ({ type: 'add', text: t }))];
  const dp = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--)
    dp[i][j] = A[i] === B[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out = []; let i = 0, j = 0;
  while (i < n && j < m) {
    if (A[i] === B[j]) { out.push({ type: 'same', text: A[i] }); i++; j++; }
    else if (dp[i + 1][j] >= dp[i][j + 1]) out.push({ type: 'del', text: A[i++] });
    else out.push({ type: 'add', text: B[j++] });
  }
  while (i < n) out.push({ type: 'del', text: A[i++] });
  while (j < m) out.push({ type: 'add', text: B[j++] });
  return out;
}

/** Side-by-side JSON diff of two values. */
export function sideBySide(oldV, newV) {
  const a = oldV === undefined || oldV === null ? '' : JSON.stringify(oldV, null, 2);
  const b = newV === undefined || newV === null ? '' : JSON.stringify(newV, null, 2);
  const ops = lineDiff(a, b);
  const left = h('div', { class: 'sbs-col' }), right = h('div', { class: 'sbs-col' });
  let k = 0;
  while (k < ops.length) {
    const op = ops[k];
    if (op.type === 'same') {
      left.append(line(op.text, '')); right.append(line(op.text, '')); k++; continue;
    }
    const dels = [], adds = [];
    while (k < ops.length && ops[k].type !== 'same') { (ops[k].type === 'del' ? dels : adds).push(ops[k].text); k++; }
    const rows = Math.max(dels.length, adds.length);
    for (let r = 0; r < rows; r++) {
      left.append(r < dels.length ? line(dels[r], 'del') : line('', 'pad'));
      right.append(r < adds.length ? line(adds[r], 'add') : line('', 'pad'));
    }
  }
  return h('div', { class: 'sbs' }, left, right);
  function line(text, cls) { return h('div', { class: `sbs-line ${cls}`, html: highlightJson(text) || '&nbsp;' }); }
}

/** Very small, safe markdown subset: code fences, inline code, bold, links-as-text. */
export function renderMarkdown(text) {
  const parts = String(text || '').split(/```/);
  const frag = h('div', { class: 'md' });
  parts.forEach((part, i) => {
    if (i % 2 === 1) {
      const body = part.replace(/^[a-zA-Z0-9_-]*\n/, '');
      frag.append(h('pre', { class: 'code' }, body));
    } else {
      const html = esc(part)
        .replace(/`([^`]+)`/g, '<code>$1</code>')
        .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
        .replace(/^#{1,6} (.*)$/gm, '<strong>$1</strong>')
        .replace(/\n/g, '<br>');
      frag.append(h('span', { html }));
    }
  });
  return frag;
}

export function modal(title, body, actions = []) {
  const close = () => wrap.remove();
  const wrap = h('div', { class: 'modal-wrap', onclick: (e) => { if (e.target === wrap) close(); } },
    h('div', { class: 'modal' },
      h('div', { class: 'modal-head' }, h('h3', {}, title), h('button', { class: 'ghost', onclick: close }, '✕')),
      h('div', { class: 'modal-body' }, body),
      actions.length ? h('div', { class: 'modal-actions' }, actions.map((a) =>
        h('button', { class: a.kind || '', onclick: async () => { if ((await a.onClick?.()) !== false) close(); } }, a.label))) : null));
  document.body.append(wrap);
  return close;
}

export function select(options, value, onchange, attrs = {}) {
  const s = h('select', { ...attrs, onchange: (e) => onchange?.(e.target.value) },
    options.map((o) => {
      const [v, label] = Array.isArray(o) ? o : [o, o];
      return h('option', { value: v, selected: v === value }, label);
    }));
  return s;
}

export const STRATEGIES = [
  ['three_way', '3-way (fail on conflict)'],
  ['semantic_llm', 'Semantic (LLM-assisted)'],
  ['deterministic', 'Deterministic (CouchDB-style)'],
  ['ours', 'Ours (keep target)'],
  ['theirs', 'Theirs (take source)'],
];

const sseKinds = new Set(['commit', 'reply', 'llm_delta', 'mailbox_updated', 'mailbox_enqueued', 'execution_started',
  'execution_finished', 'delta']);
/** Let UI extensions listen for their own named SSE events in subscribe(). */
export function registerEventKinds(...kinds) { for (const k of kinds) sseKinds.add(k); }

/** Subscribe to an SSE endpoint; returns a close function. */
export function subscribe(path, token, handler) {
  const url = `/api${path}${token ? `${path.includes('?') ? '&' : '?'}token=${encodeURIComponent(token)}` : ''}`;
  const es = new EventSource(url);
  for (const k of sseKinds) es.addEventListener(k, (e) => { try { handler(k, JSON.parse(e.data)); } catch (err) { console.error(err); } });
  return () => es.close();
}

/** Parse an SSE response body (fetch streaming), calling handler(kind, data). */
export async function readSse(res, handler) {
  const reader = res.body.getReader();
  const dec = new TextDecoder();
  let buf = '';
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let i;
    while ((i = buf.indexOf('\n\n')) >= 0) {
      const block = buf.slice(0, i); buf = buf.slice(i + 2);
      let kind = 'message'; const data = [];
      for (const line of block.split('\n')) {
        if (line.startsWith('event:')) kind = line.slice(6).trim();
        else if (line.startsWith('data:')) data.push(line.slice(5).trimStart());
      }
      if (data.length) { try { handler(kind, JSON.parse(data.join('\n'))); } catch (e) { console.error(e); } }
    }
  }
}

export function b64ToBlob(b64, type) {
  const bin = atob(b64); const arr = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
  return new Blob([arr], { type });
}

/** Sequential audio playback queue. */
export function audioQueue() {
  const queue = []; let playing = false;
  const next = () => {
    if (playing || !queue.length) return;
    playing = true;
    const url = queue.shift();
    const a = new Audio(url);
    const done = () => { playing = false; URL.revokeObjectURL(url); next(); };
    a.onended = done; a.onerror = done;
    a.play().catch(done);
  };
  return { push(blob) { queue.push(URL.createObjectURL(blob)); next(); }, clear() { queue.length = 0; } };
}

export function metersBetween(a, b) {
  const R = 6371000, t = Math.PI / 180;
  const dLat = (b.lat - a.lat) * t, dLon = (b.lon - a.lon) * t;
  const x = Math.sin(dLat / 2) ** 2 + Math.cos(a.lat * t) * Math.cos(b.lat * t) * Math.sin(dLon / 2) ** 2;
  return 2 * R * Math.asin(Math.sqrt(x));
}
