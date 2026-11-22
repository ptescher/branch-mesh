// Sessions: streaming, event-sourced chat on any branch, with any persona.
import { api, h, clear, onEvent, debounce, renderMarkdown, jsonView, colorFor, fmtTime, statusOf, toast, select, modal, badge, mediaImage, imageAttacher } from './lib.js';

export function renderBlocks(blocks) {
  const out = [];
  for (const b of Array.isArray(blocks) ? blocks : [{ type: 'text', text: String(blocks ?? '') }]) {
    switch (b.type) {
      case 'text': out.push(renderMarkdown(b.text)); break;
      case 'image':
        out.push(b.source?.type === 'media' ? mediaImage(b.source.hash)
          : b.source?.type === 'base64' ? h('img', { class: 'media-img', alt: 'image', src: `data:${b.source.media_type};base64,${b.source.data}` })
            : h('div', { class: 'blk notice' }, '🖼 image'));
        break;
      case 'thinking':
        out.push(b.thinking
          ? h('details', { class: 'blk thinking' }, h('summary', {}, '💭 reasoning summary'), renderMarkdown(b.thinking))
          : h('div', { class: 'blk thinking-chip' }, '💭 thought'));
        break;
      case 'redacted_thinking': out.push(h('div', { class: 'blk thinking-chip' }, '💭 (redacted)')); break;
      case 'tool_use':
        out.push(h('details', { class: 'blk tool' }, h('summary', {}, '🔧 ', h('b', {}, b.name), h('span', { class: 'muted mono' }, ` ${b.id || ''}`)), jsonView(b.input)));
        break;
      case 'server_tool_use':
        out.push(h('details', { class: 'blk tool server' }, h('summary', {}, '🌐 ', h('b', {}, b.name), ' (server tool)'), jsonView(b.input)));
        break;
      case 'tool_result': {
        const content = Array.isArray(b.content) ? b.content.map((c) => c.text ?? JSON.stringify(c)).join('\n') : b.content;
        out.push(h('details', { class: `blk result ${b.is_error ? 'error' : ''}` },
          h('summary', {}, b.is_error ? '⚠ tool error' : '↩ tool result', h('span', { class: 'muted mono' }, ` ${b.tool_use_id || ''}`)),
          h('pre', { class: 'json' }, String(content ?? ''))));
        break;
      }
      case 'fallback':
        out.push(h('div', { class: 'blk notice' }, `↪ model fallback: ${b.from?.model} declined, ${b.to?.model} continued`));
        break;
      default:
        if (String(b.type).endsWith('_tool_result')) {
          out.push(h('details', { class: 'blk result server' }, h('summary', {}, `🌐 ${b.type}`), jsonView(b.content, { maxHeight: '300px' })));
        } else {
          out.push(h('details', { class: 'blk' }, h('summary', {}, b.type), jsonView(b)));
        }
    }
  }
  return out;
}

/**
 * Render a stored message. `names` maps author/persona ids to display names;
 * `opts.systemLabel(m)` may return a custom heading for role:"system" records.
 */
export function renderMessage(m, names = {}, opts = {}) {
  if (m.role === 'system') {
    const label = opts.systemLabel?.(m)
      || (m.kind === 'memory_snapshot' ? '🧠 Standing facts shown to the AI'
        : `⚙ ${m.kind || 'system'}${m.author_name ? ` from ${m.author_name}` : ''} — shown to the AI`);
    return h('div', { class: `msg system ${m.kind || ''}` },
      h('div', { class: 'msg-head' }, h('span', { class: 'who' }, label), h('span', { class: 'muted small' }, fmtTime(m.ts))),
      h('div', { class: 'msg-body' }, renderMarkdown(typeof m.content === 'string' ? m.content : JSON.stringify(m.content))));
  }
  const rawWho = m.role === 'assistant' ? (m.persona || 'agent') : (m.kind === 'tool_results' ? 'tool results' : (m.author || 'user'));
  const who = names[rawWho] || rawWho;
  const side = m.role === 'assistant' || m.kind === 'tool_results' ? 'left' : 'right';
  const color = m.role === 'assistant' ? colorFor(m.persona) : undefined;
  const meta = [fmtTime(m.ts)];
  if (m.model) meta.push(m.model);
  if (m.stop_reason && m.stop_reason !== 'end_turn') meta.push(m.stop_reason);
  if (m.role !== 'assistant' && m.persona && m.kind !== 'tool_results') meta.push(`→ ${names[m.persona] || m.persona}`);
  if (m.modality) meta.push(m.modality);
  return h('div', { class: `msg ${side} ${m.kind === 'tool_results' ? 'tools' : ''} ${m.consult_of ? 'consult' : ''}` },
    h('div', { class: 'msg-head' }, h('span', { class: 'who', style: color ? { color } : {} }, who),
      m.consult_of ? badge(`consultation for ${m.consult_of}`, 'purple') : null,
      h('span', { class: 'muted small' }, meta.join(' · '))),
    h('div', { class: 'msg-body' }, renderBlocks(m.content)));
}

export function mount(root, params) {
  let branch = params.branch || '';
  let branches = [], personas = [], showClosed = false;
  const streams = new Map(); // execution -> {persona, text}
  const waiting = new Map(); // execution -> {envelope, persona, status}

  const side = h('aside', { class: 'side' });
  const main = h('section', { class: 'chat-main' });
  root.append(side, main);

  async function loadLists() {
    [branches, personas] = await Promise.all([api('/branches'), api('/personas')]);
    if (!branch) branch = (branches.find((b) => b.name === 'threads/welcome') || branches.find((b) => b.meta?.kind === 'thread') || branches[0])?.name || '';
    renderSide();
  }

  function renderSide() {
    clear(side);
    side.append(h('div', { class: 'side-head' }, h('h3', {}, 'Sessions'), h('button', { class: 'primary small', onclick: newThread }, '+ New')));
    const visible = branches.filter((b) => showClosed || statusOf(b) === 'active' || b.name === branch);
    const groups = [
      ['Threads', visible.filter((b) => b.meta?.kind === 'thread')],
      ['Forks & consultations', visible.filter((b) => b.meta?.kind !== 'thread')],
    ];
    for (const [title, list] of groups) {
      if (!list.length) continue;
      side.append(h('div', { class: 'side-group' }, title));
      for (const b of list.sort((x, y) => y.updated_at - x.updated_at)) {
        side.append(h('a', {
          class: `side-item ${b.name === branch ? 'active' : ''} st-${statusOf(b)}`,
          href: `#/chat?branch=${encodeURIComponent(b.name)}`,
          title: b.name,
        }, h('span', { class: 'mono' }, b.name), statusOf(b) !== 'active' ? badge(statusOf(b), statusOf(b) === 'merged' ? 'purple' : '') : null));
      }
    }
    side.append(h('label', { class: 'toggle small' }, h('input', { type: 'checkbox', checked: showClosed, onchange: (e) => { showClosed = e.target.checked; renderSide(); } }), ' show merged/abandoned'));
  }

  function newThread() {
    const pSel = select(personas.map((p) => [p.id, `${p.name} (${p.id})`]), personas[0]?.id);
    const idIn = h('input', { placeholder: 'thread id (optional)' });
    modal('New session', h('div', { class: 'form' }, h('label', {}, 'Persona', pSel), h('label', {}, 'Thread id', idIn)), [
      { label: 'Cancel' },
      { label: 'Create', kind: 'primary', onClick: async () => {
        const b = await api('/threads', { method: 'POST', body: { persona: pSel.value, thread_id: idIn.value.trim() || undefined } });
        location.hash = `#/chat?branch=${encodeURIComponent(b.name)}`;
      } },
    ]);
  }

  let transcript, composer, personaSel, statusLine;
  async function renderMain() {
    clear(main);
    const info = branches.find((b) => b.name === branch);
    if (!info) { main.append(h('div', { class: 'empty' }, branches.length ? 'Select a session.' : 'No branches yet — create a session.')); return; }
    const defaultPersona = info.meta?.persona || personas[0]?.id;
    personaSel = select(personas.map((p) => [p.id, p.name || p.id]), defaultPersona, null, { title: 'Persona that will receive your message' });
    main.append(h('div', { class: 'chat-head' },
      h('div', {}, h('div', { class: 'mono big' }, branch),
        h('div', { class: 'muted small' }, info.parent ? `forked from ${info.parent}` : 'root branch', ' · ', statusOf(info))),
      h('div', { class: 'btnrow' },
        h('label', { class: 'inline' }, 'Talk to ', personaSel),
        h('button', { onclick: forkHere, title: 'O(1) fork of this branch' }, '⑂ Fork'),
        info.parent ? h('button', { onclick: () => window.bmNavigate('branches', { name: branch, merge: info.parent }) }, '⇢ Merge into parent') : null,
        h('button', { onclick: () => window.bmNavigate('branches', { name: branch }) }, 'Inspect'),
        h('button', { onclick: () => window.bmNavigate('journal', { branch }) }, 'Journal'))));
    transcript = h('div', { class: 'transcript' });
    statusLine = h('div', { class: 'status-line' });
    const input = h('textarea', { placeholder: 'Message… (Enter to send, Shift+Enter for newline)', rows: 2 });
    input.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); } });
    const images = imageAttacher({ pasteTarget: input });
    const send = async () => {
      const content = input.value.trim();
      const attachments = images.hashes();
      if (images.busy()) return toast('Still uploading…', 'warn');
      if (!content && !attachments.length) return;
      input.value = '';
      try {
        const res = await api('/branch/send', { method: 'POST', body: { branch, persona: personaSel.value, content, attachments } });
        images.clear();
        waiting.set(res.envelope.execution, { envelope: res.envelope.id, persona: personaSel.value, status: 'queued' });
        renderStatus();
      } catch { input.value = content; }
    };
    composer = h('div', { class: 'composer' }, images.tray, images.button, input, h('button', { class: 'primary', onclick: send }, 'Send'));
    main.append(transcript, statusLine, composer);
    if (statusOf(info) !== 'active') composer.prepend(h('div', { class: 'muted small' }, `This branch is ${statusOf(info)}; messages still work but it will not be merged again automatically.`));
    await loadMessages();
  }

  async function loadMessages() {
    if (!transcript) return;
    let msgs;
    try { msgs = await api(`/branch/messages?name=${encodeURIComponent(branch)}`, { quiet: true }); } catch { return; }
    const atBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 80;
    clear(transcript);
    if (!msgs.length) transcript.append(h('div', { class: 'empty' }, 'No messages yet. Try “remember …”, “ask fact-checker …”, or anything else.'));
    // Collapse side conversations merged in from consultations.
    for (let i = 0; i < msgs.length; i++) {
      const m = msgs[i];
      if (!m.consult_of) { transcript.append(renderMessage(m)); continue; }
      const group = [];
      while (i < msgs.length && msgs[i].consult_of === m.consult_of && msgs[i].branch === m.branch) group.push(msgs[i++]);
      i--;
      const target = group.find((g) => g.role === 'assistant')?.persona || 'agent';
      transcript.append(h('details', { class: 'consult-group' },
        h('summary', {}, '⑂ ', h('b', {}, `${m.consult_of} consulted ${target}`), h('span', { class: 'muted small' }, ` · ${group.length} message(s) on `),
          h('a', { class: 'mono small', href: `#/chat?branch=${encodeURIComponent(m.branch)}` }, m.branch), h('span', { class: 'muted small' }, ' · merged')),
        group.map((m) => renderMessage(m))));
    }
    for (const [exec, s] of streams) {
      transcript.append(h('div', { class: 'msg left streaming', 'data-exec': exec },
        h('div', { class: 'msg-head' }, h('span', { class: 'who', style: { color: colorFor(s.persona) } }, s.persona), h('span', { class: 'muted small' }, 'streaming…')),
        h('div', { class: 'msg-body' }, renderMarkdown(s.text))));
    }
    if (atBottom || streams.size) transcript.scrollTop = transcript.scrollHeight;
  }

  function renderStatus() {
    if (!statusLine) return;
    clear(statusLine);
    for (const [, w] of waiting) {
      statusLine.append(h('div', { class: `pending ${w.status === 'error' ? 'err' : ''}` },
        w.status === 'error' ? '☠ ' : h('span', { class: 'spinner' }), `${w.persona}: ${w.status}`, w.error ? ` — ${w.error}` : ''));
    }
  }

  function forkHere() {
    const nameIn = h('input', { value: `${branch}/fork-${Math.random().toString(36).slice(2, 6)}` });
    modal('Fork branch', h('div', { class: 'form' }, h('p', { class: 'muted' }, 'Creates a new branch pointing at the current head. No data is copied (copy-on-write).'), h('label', {}, 'New branch name', nameIn)), [
      { label: 'Cancel' },
      { label: 'Fork', kind: 'primary', onClick: async () => {
        const b = await api('/branch/fork', { method: 'POST', body: { source: branch, name: nameIn.value.trim() } });
        toast(`Forked ${b.name}`, 'ok');
        location.hash = `#/chat?branch=${encodeURIComponent(b.name)}`;
      } },
    ]);
  }

  const reloadMsgs = debounce(loadMessages, 120);
  const reloadLists = debounce(async () => { [branches] = await Promise.all([api('/branches', { quiet: true })]); renderSide(); }, 500);
  const off = onEvent((ev) => {
    const d = ev.data || {};
    if (ev.kind === 'llm_delta' && d.branch === branch) {
      const s = streams.get(d.execution) || { persona: d.persona, text: '' };
      s.text += d.text; streams.set(d.execution, s);
      const w = waiting.get(d.execution); if (w) { w.status = 'responding'; renderStatus(); }
      const el = transcript?.querySelector(`[data-exec="${d.execution}"] .msg-body`);
      if (el) { el.replaceChildren(renderMarkdown(s.text)); transcript.scrollTop = transcript.scrollHeight; } else reloadMsgs();
    } else if (ev.kind === 'commit' && d.branch === branch) {
      if (d.execution) streams.delete(d.execution);
      reloadMsgs();
    } else if (ev.kind === 'execution_started' && d.execution?.branch === branch) {
      const w = waiting.get(d.execution.id); if (w) { w.status = d.replaying ? `resuming (replaying ${d.replaying} journal entries)` : 'thinking'; renderStatus(); }
    } else if (ev.kind === 'journal' && waiting.has(d.execution)) {
      const w = waiting.get(d.execution); const e = d.entry;
      if (e.kind === 'tool_call' || e.kind === 'message_sent' || e.kind === 'http_call') { w.status = `ran ${e.name.split(':')[1] || e.name}`; renderStatus(); }
    } else if (ev.kind === 'execution_finished' && waiting.has(d.execution?.id)) {
      streams.delete(d.execution.id);
      if (d.execution.status === 'completed') waiting.delete(d.execution.id);
      renderStatus(); reloadMsgs();
    } else if (ev.kind === 'mailbox_updated') {
      const env = d.envelope; const w = waiting.get(env.execution);
      if (w && env.status === 'dead_letter') { w.status = 'error'; w.error = env.last_error; renderStatus(); }
      else if (w && env.status === 'pending' && env.last_error) { w.status = `retrying after error (attempt ${env.attempts})`; w.error = env.last_error; renderStatus(); }
    } else if (['branch_created', 'branch_updated', 'branch_deleted', 'merge'].includes(ev.kind)) {
      reloadLists();
      if (ev.kind === 'merge' && d.target === branch) reloadMsgs();
    }
  });

  (async () => { await loadLists(); await renderMain(); })();
  return off;
}
