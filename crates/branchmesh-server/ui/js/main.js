import { api, connectEvents, onEvent, h, clear, debounce, badge, registerEventKinds } from './lib.js';
import * as topology from './topology.js';
import * as chat from './chat.js';
import * as branches from './branches.js';
import * as journal from './journal.js';
import * as mailbox from './mailbox.js';
import * as personas from './personas.js';
import extensions from './extensions.js';

const views = { topology, chat, branches, journal, mailbox, personas };
// null = operator session (everything visible); otherwise the allowed view names.
let allowed = null;
let home = 'topology';
let unmount = null;

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, '') || 'topology';
  const [name, query] = raw.split('?');
  return { name: views[name] ? name : home, params: Object.fromEntries(new URLSearchParams(query || '')) };
}

export function navigate(name, params = {}) {
  const qs = new URLSearchParams(params).toString();
  location.hash = `#/${name}${qs ? '?' + qs : ''}`;
}
window.bmNavigate = navigate;

function route() {
  let { name, params } = parseHash();
  if (allowed && !allowed.has(name)) { name = home; history.replaceState(null, '', `#/${home}`); }
  if (unmount) { try { unmount(); } catch (e) { console.error(e); } }
  const container = clear(document.getElementById('view'));
  container.className = `view view-${name}`;
  for (const a of document.querySelectorAll('#nav a')) a.classList.toggle('active', a.dataset.view === name);
  unmount = views[name].mount(container, params) || null;
}

async function refreshStats() {
  try {
    const s = await api('/stats', { quiet: true });
    const el = clear(document.getElementById('stats'));
    el.append(...[
      badge(s.provider === 'mock' ? 'LLM: mock' : `LLM: ${s.provider}`, s.provider === 'mock' ? 'warn' : 'ok'),
      h('span', { title: 'active / total branches' }, `⎇ ${s.active_branches}/${s.branches}`),
      h('span', { title: 'actors in RAM / known' }, `◉ ${s.actors_active}/${s.actors}`),
      h('span', { title: 'pending messages' }, `✉ ${(s.mailbox.pending || 0) + (s.mailbox.processing || 0)}`),
      s.mailbox.dead_letter ? badge(`☠ ${s.mailbox.dead_letter}`, 'err') : null,
      h('span', { title: 'content-addressed objects' }, `⬢ ${s.objects}`),
    ].filter(Boolean));
  } catch { /* offline */ }
}

async function loadExtensions() {
  const nav = document.getElementById('nav');
  let restriction = null;
  for (const ext of extensions) {
    Object.assign(views, ext.views || {});
    for (const { view, label } of ext.nav || []) nav.append(h('a', { href: `#/${view}`, 'data-view': view }, label));
    for (const href of ext.styles || []) document.head.append(h('link', { rel: 'stylesheet', href }));
    registerEventKinds(...(ext.eventKinds || []));
    try {
      const r = ext.init ? await ext.init() : null;
      if (r?.views) restriction = r;
    } catch (e) { console.error('extension init failed', e); }
  }
  return restriction;
}

async function start() {
  const restriction = await loadExtensions();
  if (restriction) {
    allowed = new Set(restriction.views);
    home = restriction.home || restriction.views[0];
  }
  for (const a of document.querySelectorAll('#nav a')) {
    a.style.display = !allowed || allowed.has(a.dataset.view) ? '' : 'none';
  }
  window.addEventListener('hashchange', route);
  if (!allowed) {
    connectEvents();
    const statsSoon = debounce(refreshStats, 600);
    onEvent((ev) => { if (ev.kind !== 'llm_delta') statsSoon(); });
    refreshStats();
    setInterval(refreshStats, 15000);
  } else {
    if (restriction.badge) document.getElementById('stats').append(badge(restriction.badge, 'ok'));
    document.getElementById('conn').style.display = 'none';
  }
  route();
}
start();
