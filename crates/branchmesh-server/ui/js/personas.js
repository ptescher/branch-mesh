// Personas: define agent personalities and their toolsets.
import { api, h, clear, onEvent, debounce, colorFor, toast, modal, select } from './lib.js';

const EFFORTS = [['', 'model default'], ['low', 'low'], ['medium', 'medium'], ['high', 'high'], ['xhigh', 'xhigh'], ['max', 'max']];

export function mount(root, params) {
  let personas = [], tools = [], selected = params.id || '';
  const side = h('aside', { class: 'side' });
  const main = h('section', { class: 'personas' });
  root.append(side, main);

  async function load() {
    [personas, tools] = await Promise.all([api('/personas'), api('/tools')]);
    if (!selected && personas.length) selected = personas[0].id;
    renderSide(); renderMain();
  }

  function renderSide() {
    clear(side);
    side.append(h('div', { class: 'side-head' }, h('h3', {}, 'Personas'), h('button', { class: 'primary small', onclick: () => { selected = '__new'; renderSide(); renderMain(); } }, '+ New')));
    for (const p of personas) {
      side.append(h('a', { class: `side-item ${p.id === selected ? 'active' : ''}`, href: 'javascript:void 0', onclick: () => { selected = p.id; renderSide(); renderMain(); } },
        h('span', { style: { color: colorFor(p.id) } }, '● '), p.name || p.id, h('div', { class: 'muted small mono' }, p.model)));
    }
  }

  function renderMain() {
    clear(main);
    const isNew = selected === '__new';
    const p = isNew ? { id: '', name: '', description: '', system_prompt: '', model: 'claude-opus-5-5', max_tokens: 16000, max_steps: 12, effort: null, tools: ['memory_write', 'memory_read'] }
      : personas.find((x) => x.id === selected);
    if (!p) { main.append(h('div', { class: 'empty' }, 'Create a persona to get started.')); return; }
    const f = {
      id: h('input', { value: p.id, disabled: !isNew, placeholder: 'research-bot' }),
      name: h('input', { value: p.name }),
      description: h('input', { value: p.description }),
      model: h('input', { value: p.model, class: 'mono' }),
      effort: select(EFFORTS, p.effort || ''),
      max_tokens: h('input', { type: 'number', value: p.max_tokens, min: 256 }),
      max_steps: h('input', { type: 'number', value: p.max_steps, min: 1, max: 100 }),
      system_prompt: h('textarea', { rows: 10 }),
    };
    f.system_prompt.value = p.system_prompt || '';
    const toolBoxes = tools.map((t) => {
      const cb = h('input', { type: 'checkbox', value: t.name, checked: (p.tools || []).includes(t.name) });
      return h('label', { class: 'tool-opt', title: t.description }, cb, h('b', { class: 'mono' }, t.name), t.server ? h('span', { class: 'badge' }, 'server') : null,
        h('div', { class: 'muted small' }, t.description || (t.server ? 'Executed by the model provider' : '')));
    });
    const save = async () => {
      const body = {
        id: f.id.value.trim(), name: f.name.value.trim(), description: f.description.value.trim(),
        model: f.model.value.trim(), effort: f.effort.value || null, max_tokens: Number(f.max_tokens.value) || 16000,
        max_steps: Number(f.max_steps.value) || 12, system_prompt: f.system_prompt.value,
        tools: toolBoxes.map((l) => l.querySelector('input')).filter((c) => c.checked).map((c) => c.value),
      };
      const saved = await api('/personas', { method: 'POST', body });
      toast(`Saved ${saved.id}`, 'ok'); selected = saved.id; await load();
    };
    main.append(h('div', { class: 'card' },
      h('div', { class: 'row-between' }, h('h3', {}, isNew ? 'New persona' : p.name || p.id),
        h('div', { class: 'btnrow' },
          !isNew ? h('button', { onclick: async () => {
            const b = await api('/threads', { method: 'POST', body: { persona: p.id } });
            window.bmNavigate('chat', { branch: b.name });
          } }, 'Start session') : null,
          !isNew ? h('button', { class: 'danger', onclick: () => modal('Delete persona', h('p', {}, `Delete ${p.id}? Existing branches and journals are kept.`), [
            { label: 'Cancel' },
            { label: 'Delete', kind: 'danger', onClick: async () => { await api(`/personas/${encodeURIComponent(p.id)}`, { method: 'DELETE' }); selected = ''; load(); } },
          ]) }, 'Delete') : null,
          h('button', { class: 'primary', onclick: save }, 'Save'))),
      h('div', { class: 'form grid2' },
        h('label', {}, 'Id', f.id), h('label', {}, 'Display name', f.name),
        h('label', { class: 'span2' }, 'Description', f.description),
        h('label', {}, 'Model', f.model), h('label', {}, 'Effort', f.effort),
        h('label', {}, 'Max output tokens', f.max_tokens), h('label', {}, 'Max LLM steps per message', f.max_steps),
        h('label', { class: 'span2' }, 'System prompt', f.system_prompt)),
      h('h4', {}, 'Tools'), h('div', { class: 'tools-grid' }, toolBoxes),
      h('p', { class: 'muted small' }, 'Tool calls are journaled: on crash recovery their recorded results are replayed instead of being executed again. ask_agent forks this branch, lets another persona work on the fork and merges it back.')));
  }

  const reload = debounce(load, 400);
  const off = onEvent((ev) => { if (ev.kind === 'persona_saved' || ev.kind === 'persona_deleted') reload(); });
  load();
  return off;
}
