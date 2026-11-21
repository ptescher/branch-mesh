// Mesh Topology: live force-directed graph of personas, actors and branches.
import { api, h, onEvent, debounce, short, statusOf, colorFor, ago } from './lib.js';

const SVGNS = 'http://www.w3.org/2000/svg';
const svgEl = (tag, attrs = {}) => {
  const el = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs)) if (v !== undefined && v !== null) el.setAttribute(k, v);
  return el;
};

const SPRING = { fork: 130, merge: 170, persona: 80, runs_on: 80, message: 240 };
const radius = (n) => n.type === 'persona' ? 34 : n.type === 'actor' ? 16 : Math.max(40, (n.label.length > 34 ? 34 : n.label.length) * 3.3 + 12);

export function mount(root, params) {
  const state = {
    nodes: new Map(), edges: [], alpha: 1, view: { x: 0, y: 0, k: 1 },
    showClosed: params.closed === '1', selected: null, raf: 0, drag: null,
  };

  const svg = svgEl('svg', { class: 'topo' });
  const defs = svgEl('defs');
  defs.innerHTML = `
    <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
      <path d="M 0 0 L 10 5 L 0 10 z" fill="currentColor"/></marker>
    <filter id="glow"><feGaussianBlur stdDeviation="3.5" result="b"/><feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter>`;
  svg.append(defs);
  const world = svgEl('g');
  const gEdges = svgEl('g'), gNodes = svgEl('g'), gFx = svgEl('g');
  world.append(gEdges, gNodes, gFx);
  svg.append(world);

  const panel = h('aside', { class: 'topo-panel' });
  const legend = h('div', { class: 'legend' },
    h('span', {}, h('i', { class: 'lg persona' }), 'persona'),
    h('span', {}, h('i', { class: 'lg actor' }), 'actor (bright = in RAM)'),
    h('span', {}, h('i', { class: 'lg branch' }), 'branch'),
    h('span', {}, h('i', { class: 'lg fork' }), 'fork'),
    h('span', {}, h('i', { class: 'lg merge' }), 'merged into'),
    h('span', {}, h('i', { class: 'lg msg' }), 'agent → agent'),
    h('label', { class: 'toggle' }, h('input', { type: 'checkbox', checked: state.showClosed, onchange: (e) => { state.showClosed = e.target.checked; load(); } }), ' show merged/abandoned'),
  );
  const stage = h('div', { class: 'topo-stage' }, svg, legend);
  root.append(stage, panel);
  renderPanel();

  function resize() {
    const r = stage.getBoundingClientRect();
    svg.setAttribute('width', r.width); svg.setAttribute('height', r.height);
    state.w = r.width; state.h = r.height;
  }
  resize();
  const ro = new ResizeObserver(resize); ro.observe(stage);

  async function load() {
    let topo;
    try { topo = await api('/topology', { quiet: true }); } catch { return; }
    const hidden = new Set();
    for (const n of topo.nodes) if (n.type === 'branch' && !state.showClosed && n.status !== 'active') hidden.add(n.id);
    // Hide actors whose branch is hidden.
    for (const n of topo.nodes) if (n.type === 'actor' && hidden.has(`b:${n.branch}`)) hidden.add(n.id);
    const next = new Map();
    for (const n of topo.nodes) {
      if (hidden.has(n.id)) continue;
      const prev = state.nodes.get(n.id);
      const near = topo.edges.find((e) => (e.target === n.id && state.nodes.has(e.source)) || (e.source === n.id && state.nodes.has(e.target)));
      const anchor = near ? state.nodes.get(near.target === n.id ? near.source : near.target) : null;
      next.set(n.id, Object.assign(prev || {
        x: (anchor?.x ?? state.w / 2) + (Math.random() - 0.5) * 80,
        y: (anchor?.y ?? state.h / 2) + (Math.random() - 0.5) * 80,
        vx: 0, vy: 0,
      }, n));
    }
    const changed = next.size !== state.nodes.size || [...next.keys()].some((k) => !state.nodes.has(k));
    state.nodes = next;
    state.edges = topo.edges.filter((e) => next.has(e.source) && next.has(e.target));
    if (changed) state.alpha = Math.max(state.alpha, 0.6);
    draw();
    renderPanel();
    kick();
  }

  // ------------------------------------------------------------ simulation
  function step() {
    const nodes = [...state.nodes.values()];
    const a = state.alpha;
    for (let i = 0; i < nodes.length; i++) {
      const n = nodes[i];
      for (let j = i + 1; j < nodes.length; j++) {
        const m = nodes[j];
        let dx = n.x - m.x, dy = n.y - m.y;
        let d2 = dx * dx + dy * dy;
        if (d2 < 1) { dx = Math.random(); dy = Math.random(); d2 = 1; }
        if (d2 > 360000) continue;
        const d = Math.sqrt(d2);
        let f = (7000 / d2) * a;
        // Hard-ish collision: wide branch labels must not overlap.
        const minD = radius(n) + radius(m);
        if (d < minD) f += (minD - d) * 0.08;
        n.vx += (dx / d) * f; n.vy += (dy / d) * f;
        m.vx -= (dx / d) * f; m.vy -= (dy / d) * f;
      }
    }
    for (const e of state.edges) {
      const s = state.nodes.get(e.source), t = state.nodes.get(e.target);
      const dx = t.x - s.x, dy = t.y - s.y;
      const d = Math.sqrt(dx * dx + dy * dy) || 1;
      const rest = SPRING[e.type] || 100;
      const k = e.type === 'message' ? 0.01 : 0.05;
      const f = (d - rest) * k * a;
      s.vx += (dx / d) * f; s.vy += (dy / d) * f;
      t.vx -= (dx / d) * f; t.vy -= (dy / d) * f;
    }
    const cx = state.w / 2, cy = state.h / 2;
    for (const n of nodes) {
      n.vx += (cx - n.x) * 0.004 * a; n.vy += (cy - n.y) * 0.004 * a;
      if (n === state.drag?.node) { n.vx = n.vy = 0; continue; }
      n.vx *= 0.82; n.vy *= 0.82;
      n.x += n.vx; n.y += n.vy;
    }
    state.alpha *= 0.985;
  }

  function kick() {
    if (state.raf) return;
    const loop = () => {
      step(); position();
      if (state.alpha > 0.02 || state.drag) state.raf = requestAnimationFrame(loop);
      else state.raf = 0;
    };
    state.raf = requestAnimationFrame(loop);
  }

  // ------------------------------------------------------------ rendering
  function nodeShape(n) {
    const g = svgEl('g', { class: `node ${n.type}`, 'data-id': n.id });
    if (n.type === 'persona') {
      const c = colorFor(n.persona);
      g.append(svgEl('circle', { r: 22, fill: c, 'fill-opacity': 0.18, stroke: c, 'stroke-width': 2 }));
      const glyph = svgEl('text', { y: 5, 'text-anchor': 'middle', class: 'glyph', fill: c }); glyph.textContent = '☺'; g.append(glyph);
      const t = svgEl('text', { y: 38, 'text-anchor': 'middle', class: 'lbl' }); t.textContent = n.label; g.append(t);
    } else if (n.type === 'actor') {
      const c = colorFor(n.persona);
      const r = svgEl('circle', { r: 9, fill: c, 'fill-opacity': n.active ? 0.95 : 0.15, stroke: c, 'stroke-width': 1.5 });
      if (n.active) r.setAttribute('filter', 'url(#glow)');
      if (n.busy) g.classList.add('busy');
      g.append(r);
      if (n.pending) { const t = svgEl('text', { x: 12, y: -8, class: 'pill' }); t.textContent = `✉${n.pending}`; g.append(t); }
      if (n.dead_letter) { const t = svgEl('text', { x: 12, y: 14, class: 'pill err' }); t.textContent = `☠${n.dead_letter}`; g.append(t); }
    } else {
      const label = n.label.length > 34 ? '…' + n.label.slice(-33) : n.label;
      const w = Math.max(60, label.length * 6.6 + 18);
      g.append(svgEl('rect', { x: -w / 2, y: -12, width: w, height: 24, rx: 6, class: `b-${n.status}` }));
      const t = svgEl('text', { y: 4, 'text-anchor': 'middle', class: 'lbl mono' }); t.textContent = label; g.append(t);
    }
    if (state.selected === n.id) g.classList.add('selected');
    g.addEventListener('pointerdown', (e) => startDrag(e, n));
    g.addEventListener('click', (e) => { e.stopPropagation(); state.selected = n.id; draw(); renderPanel(); });
    return g;
  }

  function draw() {
    gEdges.replaceChildren(); gNodes.replaceChildren();
    for (const e of state.edges) {
      const p = svgEl('path', { class: `edge ${e.type}`, 'marker-end': ['fork', 'merge', 'message'].includes(e.type) ? 'url(#arrow)' : null });
      if (e.type === 'message') p.setAttribute('stroke-width', Math.min(6, 1.5 + e.count * 0.5));
      e.el = p; gEdges.append(p);
    }
    for (const n of state.nodes.values()) { n.el = nodeShape(n); gNodes.append(n.el); }
    position();
  }

  function position() {
    world.setAttribute('transform', `translate(${state.view.x},${state.view.y}) scale(${state.view.k})`);
    for (const n of state.nodes.values()) n.el?.setAttribute('transform', `translate(${n.x},${n.y})`);
    for (const e of state.edges) {
      const s = state.nodes.get(e.source), t = state.nodes.get(e.target);
      if (!e.el) continue;
      if (e.type === 'merge' || e.type === 'message') {
        const mx = (s.x + t.x) / 2 + (t.y - s.y) * 0.2, my = (s.y + t.y) / 2 - (t.x - s.x) * 0.2;
        e.el.setAttribute('d', `M${s.x},${s.y} Q${mx},${my} ${t.x},${t.y}`);
      } else {
        e.el.setAttribute('d', `M${s.x},${s.y} L${t.x},${t.y}`);
      }
    }
  }

  // ------------------------------------------------------------ interaction
  function toWorld(e) {
    const r = svg.getBoundingClientRect();
    return { x: (e.clientX - r.left - state.view.x) / state.view.k, y: (e.clientY - r.top - state.view.y) / state.view.k };
  }
  function startDrag(e, node) {
    e.stopPropagation();
    svg.setPointerCapture(e.pointerId);
    state.drag = { node, moved: false };
    state.alpha = Math.max(state.alpha, 0.3); kick();
  }
  svg.addEventListener('pointerdown', (e) => {
    state.pan = { x: e.clientX, y: e.clientY, vx: state.view.x, vy: state.view.y };
    svg.setPointerCapture(e.pointerId);
  });
  svg.addEventListener('pointermove', (e) => {
    if (state.drag) { const p = toWorld(e); state.drag.node.x = p.x; state.drag.node.y = p.y; state.drag.moved = true; position(); }
    else if (state.pan) { state.view.x = state.pan.vx + e.clientX - state.pan.x; state.view.y = state.pan.vy + e.clientY - state.pan.y; position(); }
  });
  const endPointer = () => { state.drag = null; state.pan = null; };
  svg.addEventListener('pointerup', endPointer);
  svg.addEventListener('pointercancel', endPointer);
  svg.addEventListener('click', () => { state.selected = null; draw(); renderPanel(); });
  svg.addEventListener('wheel', (e) => {
    e.preventDefault();
    const r = svg.getBoundingClientRect();
    const mx = e.clientX - r.left, my = e.clientY - r.top;
    const k = Math.min(3, Math.max(0.25, state.view.k * (e.deltaY < 0 ? 1.1 : 0.9)));
    state.view.x = mx - ((mx - state.view.x) * k) / state.view.k;
    state.view.y = my - ((my - state.view.y) * k) / state.view.k;
    state.view.k = k; position();
  }, { passive: false });

  // ------------------------------------------------------------ effects
  function pulseNode(id, cls = 'flash') {
    const n = state.nodes.get(id);
    if (!n?.el) return;
    n.el.classList.remove(cls); void n.el.getBBox(); n.el.classList.add(cls);
    setTimeout(() => n.el?.classList.remove(cls), 900);
  }
  function travel(fromId, toId, color = 'var(--orange)') {
    const s = state.nodes.get(fromId), t = state.nodes.get(toId);
    if (!s || !t) return;
    const dot = svgEl('circle', { r: 5, fill: color, filter: 'url(#glow)' });
    gFx.append(dot);
    const t0 = performance.now();
    const anim = (now) => {
      const p = Math.min(1, (now - t0) / 700);
      dot.setAttribute('cx', s.x + (t.x - s.x) * p); dot.setAttribute('cy', s.y + (t.y - s.y) * p);
      if (p < 1) requestAnimationFrame(anim); else dot.remove();
    };
    requestAnimationFrame(anim);
  }

  // ------------------------------------------------------------ side panel
  async function renderPanel() {
    panel.replaceChildren();
    const n = state.selected && state.nodes.get(state.selected);
    if (!n) {
      const counts = { persona: 0, actor: 0, branch: 0 };
      for (const x of state.nodes.values()) counts[x.type]++;
      const active = [...state.nodes.values()].filter((x) => x.type === 'actor' && x.active).length;
      panel.append(h('h3', {}, 'Mesh'),
        h('p', { class: 'muted' }, 'Personas spawn actors (one per branch they work on). Actors live in RAM only while processing and hibernate to disk when idle. Forks and merges are shown between branches.'),
        h('div', { class: 'kv' },
          h('span', {}, 'Personas'), h('b', {}, counts.persona),
          h('span', {}, 'Actors (in RAM)'), h('b', {}, `${counts.actor} (${active})`),
          h('span', {}, 'Branches shown'), h('b', {}, counts.branch)),
        h('p', { class: 'muted small' }, 'Drag nodes, scroll to zoom, drag background to pan. Click a node for details.'));
      return;
    }
    if (n.type === 'branch') {
      const name = n.label;
      panel.append(h('h3', {}, 'Branch'), h('div', { class: 'mono big' }, name),
        h('div', { class: 'kv' },
          h('span', {}, 'Status'), h('b', {}, n.status),
          h('span', {}, 'Kind'), h('b', {}, n.kind || '—'),
          h('span', {}, 'Head'), h('b', { class: 'mono' }, short(n.head)),
          h('span', {}, 'Updated'), h('b', {}, ago(n.updated_at))),
        h('div', { class: 'btnrow' },
          h('button', { onclick: () => window.bmNavigate('chat', { branch: name }) }, 'Open session'),
          h('button', { onclick: () => window.bmNavigate('branches', { name }) }, 'Inspect')));
    } else if (n.type === 'actor') {
      panel.append(h('h3', {}, 'Actor'), h('div', { class: 'mono big' }, `${n.persona}@${n.branch}`),
        h('div', { class: 'kv' },
          h('span', {}, 'State'), h('b', {}, n.busy ? 'processing' : n.active ? 'active (idle)' : 'hibernating'),
          h('span', {}, 'Queue'), h('b', {}, n.pending),
          h('span', {}, 'Dead letters'), h('b', {}, n.dead_letter)),
        h('div', { class: 'btnrow' },
          h('button', { onclick: () => window.bmNavigate('mailbox', { actor: `${n.persona}@${n.branch}` }) }, 'Mailbox'),
          h('button', { onclick: () => window.bmNavigate('journal', { branch: n.branch }) }, 'Journal')));
    } else {
      panel.append(h('h3', {}, 'Persona'), h('div', { class: 'big' }, n.label), h('div', { class: 'mono muted' }, n.persona));
      try {
        const p = await api(`/personas/${encodeURIComponent(n.persona)}`, { quiet: true });
        panel.append(h('p', { class: 'muted' }, p.description || ''), h('div', { class: 'kv' },
          h('span', {}, 'Model'), h('b', { class: 'mono' }, p.model),
          h('span', {}, 'Tools'), h('b', {}, (p.tools || []).join(', ') || '—')));
      } catch { /* ignore */ }
      panel.append(h('div', { class: 'btnrow' }, h('button', { onclick: () => window.bmNavigate('personas', { id: n.persona }) }, 'Edit')));
    }
  }

  const reload = debounce(load, 350);
  const off = onEvent((ev) => {
    const d = ev.data || {};
    switch (ev.kind) {
      case 'commit': pulseNode(`b:${d.branch}`); break;
      case 'actor_activated': case 'actor_hibernated': case 'branch_created': case 'branch_updated':
      case 'branch_deleted': case 'persona_saved': case 'persona_deleted': case 'mailbox_enqueued': reload(); break;
      case 'mailbox_updated': reload(); break;
      case 'merge': travel(`b:${d.source}`, `b:${d.target}`, 'var(--purple)'); reload(); break;
      case 'consultation_started': travel(`p:${d.from}`, `p:${d.to}`); break;
      case 'consultation_finished': travel(`p:${d.to}`, `p:${d.from}`, 'var(--green)'); break;
      case 'execution_started': pulseNode(`a:${d.execution?.actor}@${d.execution?.branch}`, 'flash'); break;
      default: break;
    }
  });
  load();
  const timer = setInterval(load, 20000);
  return () => { off(); clearInterval(timer); ro.disconnect(); cancelAnimationFrame(state.raf); };
}
