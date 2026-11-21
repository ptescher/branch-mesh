// Timeline Replay Debugger: step through an execution's durable journal.
import { api, h, clear, onEvent, debounce, short, ago, badge, jsonView, select, toast, modal, colorFor } from './lib.js';
import { renderBlocks, renderMessage } from './chat.js';
import { diffList } from './branches.js';

const KIND_ICON = {
  message_received: '✉', llm_completion: '🧠', tool_call: '🔧', http_call: '🌐', state_write: '💾',
  message_sent: '📨', fork: '⑂', merge: '⇢', custom: '•',
};

export function mount(root, params) {
  let executions = [], current = null, entries = [], cursor = 0, playing = null, speed = 1;
  let filter = params.branch || params.actor || '';

  const side = h('aside', { class: 'side' });
  const main = h('section', { class: 'journal' });
  root.append(side, main);

  async function loadList() {
    executions = await api('/executions?limit=500');
    renderSide();
    const pick = params.exec || (current && current.id) || executions.find((e) => !filter || e.branch === filter || e.actor === filter)?.id;
    if (pick && (!current || current.id !== pick)) open(pick);
  }

  function renderSide() {
    clear(side);
    const input = h('input', { placeholder: 'filter by actor / branch / id', value: filter, oninput: (e) => { filter = e.target.value; renderItems(); } });
    const items = h('div');
    side.append(h('div', { class: 'side-head' }, h('h3', {}, 'Executions')), input, items);
    const renderItems = () => {
      clear(items);
      const list = executions.filter((e) => !filter || e.id.includes(filter) || e.actor.includes(filter) || e.branch.includes(filter));
      if (!list.length) items.append(h('div', { class: 'muted small pad' }, 'No executions yet — send a message from Sessions.'));
      for (const e of list.slice(0, 300)) {
        items.append(h('a', {
          class: `side-item exec ${current?.id === e.id ? 'active' : ''}`,
          href: 'javascript:void 0', onclick: () => open(e.id),
        },
        h('div', { class: 'row-between' }, h('span', { style: { color: colorFor(e.actor) } }, e.actor), badge(e.status, e.status === 'completed' ? 'ok' : e.status === 'failed' ? 'err' : 'warn')),
        h('div', { class: 'mono small muted' }, e.branch),
        h('div', { class: 'small muted' }, `${e.entries} steps · ${e.attempts > 1 ? `${e.attempts} attempts · ` : ''}${ago(e.started_at)}`)));
      }
    };
    renderItems();
  }

  async function open(id) {
    stop();
    const data = await api(`/executions/${encodeURIComponent(id)}`);
    current = data.execution; entries = data.entries; cursor = Math.max(0, entries.length - 1);
    if (params.exec !== id) history.replaceState(null, '', `#/journal?exec=${encodeURIComponent(id)}`);
    renderSide(); renderMain();
  }

  let detailEl, stateEl, track, scrub, counter;
  function renderMain() {
    clear(main);
    if (!current) { main.append(h('div', { class: 'empty' }, 'Select an execution to replay.')); return; }
    const e = current;
    const dur = e.finished_at ? `${((e.finished_at - e.started_at) / 1000).toFixed(1)}s` : 'running';
    main.append(h('div', { class: 'card head-card' },
      h('div', { class: 'row-between' },
        h('div', {}, h('div', { class: 'big' }, h('span', { style: { color: colorFor(e.actor) } }, e.actor), ' @ ', h('span', { class: 'mono' }, e.branch)),
          h('div', { class: 'mono small muted' }, e.id)),
        h('div', { class: 'btnrow' }, badge(e.status, e.status === 'completed' ? 'ok' : e.status === 'failed' ? 'err' : 'warn'),
          h('button', { onclick: () => window.bmNavigate('chat', { branch: e.branch }) }, 'Open session'))),
      h('div', { class: 'kv wide' },
        h('span', {}, 'Input'), h('b', {}, `${e.input?.from}: ${String(e.input?.content || '').slice(0, 200)}`),
        h('span', {}, 'Duration'), h('b', {}, dur),
        h('span', {}, 'Attempts'), h('b', {}, `${e.attempts}${e.replayed ? ` (last attempt replayed ${e.replayed} recorded step(s) from the journal)` : ''}`),
        e.error ? h('span', {}, 'Error') : null, e.error ? h('b', { class: 'err-text' }, e.error) : null,
        e.result ? h('span', {}, 'Reply') : null, e.result ? h('b', {}, String(e.result.text || '').slice(0, 300)) : null)));

    if (!entries.length) { main.append(h('div', { class: 'empty' }, 'No journal entries.')); return; }
    track = h('div', { class: 'track' });
    const t0 = entries[0].started_at;
    const t1 = Math.max(...entries.map((x) => x.started_at + x.duration_ms), t0 + 1);
    entries.forEach((en, i) => {
      track.append(h('button', {
        class: `tick k-${en.kind}`, title: `#${en.seq} ${en.name} (${en.duration_ms}ms)`,
        style: { left: `${((en.started_at - t0) / (t1 - t0)) * 100}%`, width: `${Math.max(0.6, (en.duration_ms / (t1 - t0)) * 100)}%` },
        onclick: () => seek(i),
      }));
    });
    scrub = h('input', { type: 'range', min: 0, max: entries.length - 1, value: cursor, oninput: (ev) => seek(Number(ev.target.value)) });
    counter = h('span', { class: 'mono' });
    const playBtn = h('button', { class: 'primary', onclick: () => (playing ? stop() : play()) }, '▶ Play');
    main.append(h('div', { class: 'card replay' },
      track, scrub,
      h('div', { class: 'controls' },
        h('button', { onclick: () => seek(0), title: 'first' }, '⏮'),
        h('button', { onclick: () => seek(cursor - 1), title: 'step back (←)' }, '◀'),
        playBtn,
        h('button', { onclick: () => seek(cursor + 1), title: 'step forward (→)' }, '▶'),
        h('button', { onclick: () => seek(entries.length - 1), title: 'last' }, '⏭'),
        h('label', { class: 'inline' }, 'speed ', select([['0.5', '0.5×'], ['1', '1×'], ['2', '2×'], ['4', '4×']], String(speed), (v) => { speed = Number(v); if (playing) { stop(); play(); } })),
        counter),
      h('div', { class: 'legend small' }, Object.entries(KIND_ICON).map(([k, icon]) => h('span', {}, h('i', { class: `sw k-${k}` }), `${icon} ${k.replace('_', ' ')}`)))));
    main._playBtn = playBtn;
    detailEl = h('div', { class: 'card' });
    stateEl = h('div', { class: 'card' });
    main.append(h('div', { class: 'split even' }, detailEl, stateEl));
    seek(cursor);
  }

  function play() {
    if (cursor >= entries.length - 1) seek(0);
    playing = setInterval(() => { if (cursor >= entries.length - 1) stop(); else seek(cursor + 1); }, 900 / speed);
    if (main._playBtn) main._playBtn.textContent = '⏸ Pause';
  }
  function stop() {
    if (playing) clearInterval(playing);
    playing = null;
    if (main._playBtn) main._playBtn.textContent = '▶ Play';
  }

  const loadStateSoon = debounce((i) => loadState(i), 150);
  function seek(i) {
    if (!entries.length) return;
    cursor = Math.max(0, Math.min(entries.length - 1, i));
    scrub.value = cursor;
    counter.textContent = ` step ${cursor + 1} / ${entries.length}`;
    [...track.children].forEach((c, k) => { c.classList.toggle('active', k === cursor); c.classList.toggle('past', k < cursor); });
    renderEntry(entries[cursor]);
    loadStateSoon(cursor);
  }

  function renderEntry(en) {
    clear(detailEl);
    detailEl.append(h('div', { class: 'row-between' },
      h('h3', {}, `${KIND_ICON[en.kind] || '•'} ${en.name}`),
      h('span', { class: 'muted small' }, `#${en.seq} · ${new Date(en.started_at).toLocaleTimeString()} · ${en.duration_ms} ms`)));
    if (en.kind === 'llm_completion') {
      const out = en.output || {};
      detailEl.append(h('div', { class: 'kv wide' },
        h('span', {}, 'Model'), h('b', { class: 'mono' }, `${out.model} (${en.input?.provider || out.provider})`),
        h('span', {}, 'Stop reason'), h('b', {}, out.stop_reason),
        h('span', {}, 'Usage'), h('b', { class: 'mono' }, `in ${out.usage?.input_tokens ?? '?'} · out ${out.usage?.output_tokens ?? '?'}${out.usage?.cache_read_input_tokens ? ` · cache read ${out.usage.cache_read_input_tokens}` : ''}`),
        h('span', {}, 'Context'), h('b', {}, `${en.input?.messages} message(s) at `, h('span', { class: 'mono' }, short(en.input?.context_commit)), ` · tools: ${(en.input?.tools || []).join(', ') || 'none'}`)),
      h('h4', {}, 'Prompt tail (last message sent)'), jsonView(en.input?.last_message, { maxHeight: '220px' }),
      h('h4', {}, 'Model output'), h('div', { class: 'msg left' }, h('div', { class: 'msg-body' }, renderBlocks(out.content || []))),
      h('details', {}, h('summary', {}, 'raw output'), jsonView(out, { maxHeight: '400px' })));
    } else {
      detailEl.append(h('h4', {}, 'Input'), jsonView(en.input, { maxHeight: '260px' }), h('h4', {}, 'Output'), jsonView(en.output, { maxHeight: '320px' }));
    }
    if (en.commit) detailEl.append(h('div', { class: 'muted small' }, 'Produced commit ', h('span', { class: 'mono' }, short(en.commit))));
  }

  async function loadState(i) {
    let commit = null;
    for (let k = i; k >= 0; k--) if (entries[k].commit) { commit = entries[k].commit; break; }
    if (!commit) {
      const ctx = entries.slice(0, i + 1).reverse().find((e) => e.input?.context_commit);
      commit = ctx?.input?.context_commit || null;
    }
    clear(stateEl);
    if (!commit) { stateEl.append(h('div', { class: 'muted' }, 'No state recorded yet at this step.')); return; }
    const own = entries[i].commit === commit;
    stateEl.append(h('div', { class: 'row-between' },
      h('h3', {}, 'Branch state at this step'),
      h('div', { class: 'btnrow' }, h('button', { onclick: () => forkAt(commit) }, '⑂ Fork from here'))),
    h('div', { class: 'muted small' }, own ? 'This step wrote commit ' : 'Latest commit before this step: ', h('span', { class: 'mono' }, short(commit))));
    try {
      const [c, msgs] = await Promise.all([
        api(`/commit/${commit}`, { quiet: true }),
        api(`/branch/messages?name=${encodeURIComponent(current.branch)}&commit=${commit}`, { quiet: true }),
      ]);
      if (own) stateEl.append(h('h4', {}, 'What this step changed'), diffList(c.diff));
      stateEl.append(h('h4', {}, `Conversation as of this step (${msgs.length})`),
        h('div', { class: 'transcript mini' }, msgs.slice(-12).map(renderMessage)));
    } catch (err) { stateEl.append(h('div', { class: 'err-text' }, String(err.message))); }
  }

  function forkAt(commit) {
    const nameIn = h('input', { value: `${current.branch}/replay-${short(commit).slice(0, 6)}` });
    modal('Time-travel fork', h('div', { class: 'form' }, h('p', { class: 'muted' }, 'Start a new branch from this point of the execution to explore an alternative path.'), h('label', {}, 'Branch name', nameIn)), [
      { label: 'Cancel' },
      { label: 'Fork', kind: 'primary', onClick: async () => {
        const b = await api('/branches', { method: 'POST', body: { name: nameIn.value.trim(), at: commit, meta: { kind: 'fork', persona: current.actor } } });
        toast(`Created ${b.name}`, 'ok'); window.bmNavigate('chat', { branch: b.name });
      } },
    ]);
  }

  const onKey = (e) => {
    if (e.target.tagName === 'INPUT' || e.target.tagName === 'TEXTAREA') return;
    if (e.key === 'ArrowRight') seek(cursor + 1);
    else if (e.key === 'ArrowLeft') seek(cursor - 1);
    else if (e.key === ' ') { e.preventDefault(); playing ? stop() : play(); }
  };
  window.addEventListener('keydown', onKey);
  const refreshList = debounce(async () => { executions = await api('/executions?limit=500', { quiet: true }); renderSide(); }, 600);
  const off = onEvent((ev) => {
    const d = ev.data || {};
    if (ev.kind === 'journal' && current && d.execution === current.id) {
      const follow = cursor >= entries.length - 1;
      entries.push(d.entry);
      current.entries = entries.length;
      renderMain();
      if (follow) seek(entries.length - 1);
    } else if (ev.kind === 'execution_finished' && current && d.execution.id === current.id) {
      current = d.execution; const c = cursor; renderMain(); seek(c);
    }
    if (ev.kind === 'execution_started' || ev.kind === 'execution_finished') refreshList();
  });
  loadList();
  return () => { off(); stop(); window.removeEventListener('keydown', onKey); };
}
