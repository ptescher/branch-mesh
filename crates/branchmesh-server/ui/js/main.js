import { api, connectEvents, onEvent, h, clear, debounce, badge } from './lib.js';
import * as topology from './topology.js';
import * as chat from './chat.js';
import * as branches from './branches.js';
import * as journal from './journal.js';
import * as mailbox from './mailbox.js';
import * as personas from './personas.js';

const views = { topology, chat, branches, journal, mailbox, personas };
let unmount = null;

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, '') || 'topology';
  const [name, query] = raw.split('?');
  return { name: views[name] ? name : 'topology', params: Object.fromEntries(new URLSearchParams(query || '')) };
}

export function navigate(name, params = {}) {
  const qs = new URLSearchParams(params).toString();
  location.hash = `#/${name}${qs ? '?' + qs : ''}`;
}
window.bmNavigate = navigate;

function route() {
  const { name, params } = parseHash();
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

window.addEventListener('hashchange', route);
connectEvents();
const statsSoon = debounce(refreshStats, 600);
onEvent((ev) => { if (ev.kind !== 'llm_delta') statsSoon(); });
refreshStats();
setInterval(refreshStats, 15000);
route();
