// Actor Mailbox: per-actor load and lifecycle, pending queues and dead letters.
import { api, h, clear, onEvent, debounce, ago, badge, colorFor, jsonView, modal, toast } from './lib.js';

export function mount(root, params) {
  let status = params.status || '', actorFilter = params.actor || '';
  const wrap = h('section', { class: 'mailbox' });
  root.append(wrap);
  const actorsCard = h('div', { class: 'card' });
  const envCard = h('div', { class: 'card' });
  wrap.append(actorsCard, envCard);

  async function load() {
    const [actors, envelopes] = await Promise.all([
      api('/actors', { quiet: true }),
      api(`/mailbox${status ? `?status=${status}` : ''}`, { quiet: true }),
    ]).catch(() => [[], []]);
    renderActors(actors);
    renderEnvelopes(envelopes.filter((e) => !actorFilter || `${e.to}@${e.branch}` === actorFilter));
  }

  function renderActors(actors) {
    clear(actorsCard);
    const live = actors.filter((a) => a.active).length;
    actorsCard.append(h('div', { class: 'row-between' }, h('h3', {}, 'Actors'),
      h('span', { class: 'muted small' }, `${live} in RAM · ${actors.length - live} hibernating · idle actors hibernate automatically`)));
    if (!actors.length) { actorsCard.append(h('div', { class: 'empty small' }, 'No actors yet.')); return; }
    actorsCard.append(h('div', { class: 'table-wrap' }, h('table', {},
      h('thead', {}, h('tr', {}, ['Actor', 'State', 'Queue', 'Done', 'Dead', 'LLM calls', 'Tokens in / out', 'Avg LLM', 'Activations', 'Last active'].map((t) => h('th', {}, t)))),
      h('tbody', {}, actors.map((a) => h('tr', {
        class: actorFilter === a.key ? 'selected' : '',
        onclick: () => { actorFilter = actorFilter === a.key ? '' : a.key; load(); },
      },
      h('td', {}, h('span', { style: { color: colorFor(a.persona) } }, a.persona), h('span', { class: 'muted' }, '@'), h('span', { class: 'mono small' }, a.branch)),
      h('td', {}, a.current ? badge('processing', 'warn') : a.active ? badge('active', 'ok') : badge('hibernating')),
      h('td', {}, a.pending + a.processing),
      h('td', {}, a.done),
      h('td', {}, a.dead_letter ? badge(a.dead_letter, 'err') : '0'),
      h('td', {}, a.stats.llm_calls),
      h('td', { class: 'mono small' }, `${a.stats.input_tokens} / ${a.stats.output_tokens}`),
      h('td', {}, a.stats.llm_calls ? `${Math.round(a.stats.llm_ms / a.stats.llm_calls)} ms` : '—'),
      h('td', {}, a.stats.activations),
      h('td', { class: 'small' }, ago(a.stats.last_active)))))),
    h('div', { class: 'muted small' }, 'Token and latency counters are per process lifetime. Click an actor to filter its messages.')));
  }

  function renderEnvelopes(envs) {
    clear(envCard);
    const tabs = [['', 'All'], ['pending', 'Pending'], ['processing', 'Processing'], ['done', 'Done'], ['dead_letter', 'Dead letters']];
    envCard.append(h('div', { class: 'row-between' }, h('h3', {}, 'Messages'),
      h('div', { class: 'tabs inline' }, tabs.map(([k, label]) => h('button', { class: status === k ? 'active' : '', onclick: () => { status = k; load(); } }, label)))),
    actorFilter ? h('div', { class: 'small' }, 'Filtered to ', h('b', { class: 'mono' }, actorFilter), ' ', h('a', { href: 'javascript:void 0', onclick: () => { actorFilter = ''; load(); } }, 'clear')) : null);
    if (!envs.length) { envCard.append(h('div', { class: 'empty small' }, 'No messages.')); return; }
    envCard.append(h('div', { class: 'table-wrap' }, h('table', {},
      h('thead', {}, h('tr', {}, ['Created', 'From', 'To', 'Branch', 'Content', 'Status', 'Attempts', ''].map((t) => h('th', {}, t)))),
      h('tbody', {}, envs.slice(0, 300).map((e) => h('tr', {},
        h('td', { class: 'small' }, ago(e.created_at)),
        h('td', { class: 'small' }, e.from),
        h('td', {}, h('span', { style: { color: colorFor(e.to) } }, e.to)),
        h('td', { class: 'mono small' }, e.branch),
        h('td', { class: 'clip' }, e.content.slice(0, 120)),
        h('td', {}, badge(e.status.replace('_', ' '), { done: 'ok', dead_letter: 'err', processing: 'warn' }[e.status] || ''),
          e.last_error ? h('div', { class: 'err-text small clip', title: e.last_error }, e.last_error) : null),
        h('td', {}, e.attempts),
        h('td', { class: 'btnrow' },
          h('button', { class: 'small', onclick: () => modal(`Envelope ${e.id}`, jsonView(e)) }, 'View'),
          h('button', { class: 'small', onclick: () => window.bmNavigate('journal', { exec: e.execution }) }, 'Journal'),
          e.status === 'dead_letter' || e.status === 'pending' ? h('button', { class: 'small primary', onclick: async () => {
            await api(`/mailbox/${encodeURIComponent(e.id)}/retry`, { method: 'POST' }); toast('Re-dispatched', 'ok'); load();
          } }, 'Retry') : null)))))));
  }

  const reload = debounce(load, 300);
  const off = onEvent((ev) => {
    if (['mailbox_enqueued', 'mailbox_updated', 'actor_activated', 'actor_hibernated', 'execution_finished'].includes(ev.kind)) reload();
  });
  load();
  const timer = setInterval(load, 10000);
  return () => { off(); clearInterval(timer); };
}
