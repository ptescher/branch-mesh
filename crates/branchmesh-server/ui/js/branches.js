// Branch & Merge Inspector: branch tree, diffs, history, rollback, merges and merge requests.
import { api, h, clear, onEvent, debounce, short, ago, statusOf, badge, jsonTree, jsonView, sideBySide, select, modal, toast, STRATEGIES } from './lib.js';

export function diffList(changes) {
  if (!changes.length) return h('div', { class: 'empty small' }, 'No differences.');
  return h('div', { class: 'diff-list' }, changes.map((c) => h('details', { class: `diff-item k-${c.kind}`, open: changes.length <= 6 },
    h('summary', {}, badge(c.kind, c.kind === 'added' ? 'ok' : c.kind === 'removed' ? 'err' : 'warn'), h('span', { class: 'mono' }, c.path)),
    sideBySide(c.old, c.new))));
}

export function mount(root, params) {
  let branches = [], mrs = [], selected = params.name || '', tab = params.tab || (params.merge ? 'merge' : 'diff');
  let selectedCommit = params.commit || null;

  const side = h('aside', { class: 'side' });
  const main = h('section', { class: 'inspector' });
  root.append(side, main);

  async function load() {
    [branches, mrs] = await Promise.all([api('/branches'), api('/merge-requests')]);
    if (!selected && branches.length) selected = (branches.find((b) => b.name === 'threads/welcome') || branches[0]).name;
    renderSide();
  }

  function renderSide() {
    clear(side);
    const open = mrs.filter((m) => m.status === 'open');
    side.append(h('div', { class: 'side-head' }, h('h3', {}, 'Branches')));
    if (open.length) {
      side.append(h('div', { class: 'side-group' }, `Merge requests (${open.length} open)`));
      for (const m of open) side.append(h('a', { class: 'side-item mr', href: 'javascript:void 0', onclick: () => openMr(m) }, '⇢ ', m.title));
    }
    side.append(h('div', { class: 'side-group' }, 'Tree'));
    const byParent = new Map();
    for (const b of branches) {
      const p = b.parent && branches.some((x) => x.name === b.parent) ? b.parent : '';
      if (!byParent.has(p)) byParent.set(p, []);
      byParent.get(p).push(b);
    }
    const walk = (parent, depth) => {
      for (const b of (byParent.get(parent) || []).sort((a, c) => a.created_at - c.created_at)) {
        const st = statusOf(b);
        side.append(h('a', {
          class: `side-item st-${st} ${b.name === selected ? 'active' : ''}`,
          style: { paddingLeft: `${10 + depth * 14}px` },
          href: `#/branches?name=${encodeURIComponent(b.name)}`,
          title: b.name,
        }, depth ? h('span', { class: 'muted' }, '└ ') : null, h('span', { class: 'mono' }, depth ? b.name.replace(`${parent}/`, '') : b.name),
        st !== 'active' ? badge(st, st === 'merged' ? 'purple' : '') : null));
        walk(b.name, depth + 1);
      }
    };
    walk('', 0);
  }

  async function renderMain() {
    clear(main);
    const info = branches.find((b) => b.name === selected);
    if (!info) { main.append(h('div', { class: 'empty' }, 'Select a branch.')); return; }
    const detail = await api(`/branch?name=${encodeURIComponent(selected)}`);
    const div = detail.divergence;
    main.append(h('div', { class: 'card head-card' },
      h('div', { class: 'row-between' },
        h('div', {}, h('div', { class: 'mono big' }, info.name),
          h('div', { class: 'muted small' }, `${info.meta?.kind || 'branch'} · created ${ago(info.created_at)} · updated ${ago(info.updated_at)}`)),
        h('div', { class: 'btnrow' },
          h('button', { onclick: () => window.bmNavigate('chat', { branch: info.name }) }, 'Open session'),
          h('button', { onclick: () => forkModal(info.name) }, '⑂ Fork'),
          statusOf(info) === 'active' ? h('button', { onclick: abandon }, 'Abandon') : null,
          h('button', { class: 'danger', onclick: del }, 'Delete'))),
      h('div', { class: 'kv wide' },
        h('span', {}, 'Head'), h('b', { class: 'mono' }, short(info.head), ' ', h('span', { class: 'muted' }, detail.head_commit.message)),
        h('span', {}, 'Status'), h('b', {}, statusOf(info), info.status?.into ? ` → ${info.status.into}` : ''),
        h('span', {}, 'Parent'), h('b', { class: 'mono' }, info.parent || '—'),
        h('span', {}, 'Fork point'), h('b', { class: 'mono' }, short(info.fork_point) || '—'),
        div ? h('span', {}, 'Divergence') : null,
        div ? h('b', {}, `${div.ahead} ahead, ${div.behind} behind ${div.parent}`) : null)));

    const tabs = [['diff', 'Diff'], ['history', 'History'], ['state', 'State'], ['merge', 'Merge']];
    main.append(h('div', { class: 'tabs' }, tabs.map(([k, label]) =>
      h('button', { class: tab === k ? 'active' : '', onclick: () => { tab = k; renderMain(); } }, label))));
    const body = h('div', { class: 'tab-body' });
    main.append(body);
    if (tab === 'diff') await renderDiff(body, info);
    else if (tab === 'history') await renderHistory(body, info);
    else if (tab === 'state') await renderState(body, info);
    else await renderMerge(body, info);
  }

  // ------------------------------------------------------------ diff
  async function renderDiff(body, info) {
    const parent = branches.find((b) => b.name === info.parent);
    const opts = [];
    if (info.fork_point) opts.push([info.fork_point, `fork point (${short(info.fork_point)})`]);
    if (parent) opts.push([parent.name, `parent head (${parent.name})`]);
    const log = await api(`/branch/log?name=${encodeURIComponent(info.name)}&limit=50`);
    for (const c of log.slice(1)) opts.push([c.hash, `${short(c.hash)} · ${c.commit.message}`]);
    if (!opts.length) { body.append(h('div', { class: 'empty' }, 'Nothing to compare yet.')); return; }
    const out = h('div');
    const run = async (from) => {
      clear(out).append(h('div', { class: 'muted small' }, 'loading…'));
      const d = await api(`/diff?from=${encodeURIComponent(from)}&to=${encodeURIComponent(info.name)}`);
      clear(out).append(h('div', { class: 'muted small' }, `${d.changes.length} changed path(s): ${short(d.from)} → ${short(d.to)}`), diffList(d.changes));
    };
    body.append(h('div', { class: 'toolbar' }, h('label', { class: 'inline' }, 'Compare head against ', select(opts, opts[0][0], run))), out);
    run(opts[0][0]);
  }

  // ------------------------------------------------------------ history
  async function renderHistory(body, info) {
    const log = await api(`/branch/log?name=${encodeURIComponent(info.name)}&limit=300`);
    const list = h('div', { class: 'commit-list' });
    const detail = h('div', { class: 'commit-detail' });
    body.append(h('div', { class: 'split' }, list, detail));
    const show = async (hash) => {
      selectedCommit = hash;
      for (const r of list.children) r.classList.toggle('active', r.dataset.hash === hash);
      const c = await api(`/commit/${hash}`);
      clear(detail).append(
        h('div', { class: 'row-between' }, h('div', {}, h('div', { class: 'mono' }, hash), h('div', {}, c.commit.message)),
          h('div', { class: 'btnrow' },
            h('button', { onclick: () => rollback(hash) }, '↶ Rollback to here'),
            h('button', { onclick: () => forkModal(info.name, hash) }, '⑂ Fork from here'),
            h('button', { onclick: () => stateModal(hash) }, 'View state'))),
        h('div', { class: 'kv wide' },
          h('span', {}, 'Author'), h('b', {}, c.commit.author),
          h('span', {}, 'Time'), h('b', {}, new Date(c.commit.timestamp).toLocaleString()),
          h('span', {}, 'Parents'), h('b', { class: 'mono' }, c.commit.parents.map(short).join(', ') || '—'),
          c.commit.execution ? h('span', {}, 'Execution') : null,
          c.commit.execution ? h('b', {}, h('a', { href: `#/journal?exec=${encodeURIComponent(c.commit.execution)}` }, c.commit.execution)) : null),
        c.commit.meta ? h('details', {}, h('summary', {}, 'commit metadata'), jsonView(c.commit.meta)) : null,
        h('h4', {}, 'Changes vs first parent'), diffList(c.diff));
    };
    for (const { hash, commit } of log) {
      const merge = commit.parents.length > 1;
      list.append(h('div', { class: 'commit-row', 'data-hash': hash, onclick: () => show(hash) },
        h('span', { class: `dot ${merge ? 'merge' : commit.meta?.rollback ? 'rollback' : ''}` }),
        h('span', { class: 'mono muted' }, short(hash)),
        h('span', { class: 'msg-text' }, commit.message),
        merge ? badge('merge', 'purple') : null,
        commit.meta?.rollback ? badge('rollback', 'warn') : null,
        h('span', { class: 'muted small' }, `${commit.author} · ${ago(commit.timestamp)}`)));
    }
    show(selectedCommit && log.some((c) => c.hash === selectedCommit) ? selectedCommit : log[0].hash);
  }

  async function renderState(body, info) {
    const s = await api(`/branch/state?name=${encodeURIComponent(info.name)}`);
    body.append(h('div', { class: 'muted small' }, `state at ${short(s.commit)}`), h('div', { class: 'tree-wrap' }, jsonTree(s.state)));
  }

  function stateModal(hash) {
    api(`/commit/${hash}/state`).then((s) => modal(`State at ${short(hash)}`, h('div', { class: 'tree-wrap' }, jsonTree(s.state))));
  }

  // ------------------------------------------------------------ merge
  async function renderMerge(body, info) {
    const targets = branches.filter((b) => b.name !== info.name && statusOf(b) === 'active').map((b) => b.name);
    if (!targets.length) { body.append(h('div', { class: 'empty' }, 'No other active branch to merge into.')); return; }
    let target = params.merge && targets.includes(params.merge) ? params.merge : (info.parent && targets.includes(info.parent) ? info.parent : targets[0]);
    let strategy = 'semantic_llm';
    const out = h('div');
    const preview = async () => {
      clear(out).append(h('div', { class: 'muted small' }, 'computing preview…'));
      const p = await api('/merge/preview', { method: 'POST', body: { source: info.name, target } });
      renderPreview(out, p, () => strategy, async (resolutions) => {
        const res = await api('/merge', { method: 'POST', body: { source: info.name, target, strategy, resolutions } }).catch((e) => {
          if (e.data?.conflicts) toast(`${e.data.conflicts.length} conflict(s) need a resolution or another strategy`, 'warn');
          return null;
        });
        if (res) {
          toast(res.up_to_date ? 'Already up to date' : `Merged into ${target} (${short(res.commit)})`, 'ok');
          window.bmNavigate('branches', { name: target, tab: 'history' });
        }
      }, async () => {
        await api('/merge-requests', { method: 'POST', body: { source: info.name, target, strategy } });
        toast('Merge request opened', 'ok'); load();
      });
    };
    body.append(h('div', { class: 'toolbar' },
      h('label', { class: 'inline' }, 'Merge ', h('b', { class: 'mono' }, info.name), ' into ', select(targets, target, (v) => { target = v; preview(); })),
      h('label', { class: 'inline' }, 'strategy ', select(STRATEGIES, strategy, (v) => { strategy = v; }))), out);
    preview();
  }

  function renderPreview(out, p, getStrategy, onMerge, onRequest) {
    clear(out);
    if (p.up_to_date) { out.append(h('div', { class: 'empty' }, `${p.target} already contains everything from ${p.source}.`)); return; }
    const choices = {};
    out.append(h('div', { class: 'kv wide' },
      h('span', {}, 'Merge base'), h('b', { class: 'mono' }, short(p.base)),
      h('span', {}, 'Ours (target)'), h('b', { class: 'mono' }, short(p.ours)),
      h('span', {}, 'Theirs (source)'), h('b', { class: 'mono' }, short(p.theirs)),
      h('span', {}, 'Type'), h('b', {}, p.fast_forward ? 'fast-forward (target unchanged since fork)' : 'true 3-way merge')));
    out.append(h('h4', {}, `Clean changes (${p.changes.length})`),
      p.changes.length ? h('div', { class: 'diff-list' }, p.changes.map((c) => h('details', { class: 'diff-item' },
        h('summary', {}, badge(c.action, c.action === 'auto_merged' ? 'purple' : c.action === 'delete' ? 'err' : 'ok'), h('span', { class: 'mono' }, c.path)),
        jsonView(c.value, { maxHeight: '260px' })))) : h('div', { class: 'muted small' }, 'none'));
    out.append(h('h4', {}, `Conflicts (${p.conflicts.length})`));
    if (!p.conflicts.length) out.append(h('div', { class: 'muted small' }, 'No conflicts — any strategy merges cleanly.'));
    for (const c of p.conflicts) {
      const custom = h('textarea', { class: 'mono', rows: 6, placeholder: 'custom JSON value' });
      custom.value = JSON.stringify(c.theirs ?? c.ours, null, 2);
      const radios = h('div', { class: 'radios' }, [['strategy', 'use strategy'], ['ours', 'keep ours'], ['theirs', 'take theirs'], ['custom', 'custom']].map(([v, label]) =>
        h('label', {}, h('input', { type: 'radio', name: `r-${c.path}`, value: v, checked: v === 'strategy', onchange: () => { choices[c.path] = v; } }), label)));
      out.append(h('div', { class: 'conflict card' },
        h('div', { class: 'mono' }, '⚠ ', c.path),
        h('div', { class: 'three' },
          h('div', {}, h('div', { class: 'muted small' }, 'base'), jsonView(c.base, { maxHeight: '220px' })),
          h('div', {}, h('div', { class: 'muted small' }, 'ours (target)'), jsonView(c.ours, { maxHeight: '220px' })),
          h('div', {}, h('div', { class: 'muted small' }, 'theirs (source)'), jsonView(c.theirs, { maxHeight: '220px' }))),
        radios, h('details', {}, h('summary', {}, 'custom value'), custom)));
      c._custom = custom;
    }
    out.append(h('div', { class: 'btnrow' },
      h('button', { class: 'primary', onclick: () => {
        const resolutions = {};
        for (const c of p.conflicts) {
          const ch = choices[c.path];
          if (ch === 'ours') resolutions[c.path] = c.ours ?? null;
          else if (ch === 'theirs') resolutions[c.path] = c.theirs ?? null;
          else if (ch === 'custom') {
            try { resolutions[c.path] = JSON.parse(c._custom.value); } catch { toast(`invalid JSON for ${c.path}`, 'error'); return; }
          }
        }
        onMerge(resolutions);
      } }, 'Merge now'),
      onRequest ? h('button', { onclick: onRequest }, 'Open merge request') : null,
      h('span', { class: 'muted small' }, 'Per-conflict choices override the selected strategy.')));
  }

  async function openMr(mr) {
    const body = h('div');
    const p = await api('/merge/preview', { method: 'POST', body: { source: mr.source, target: mr.target } });
    let strategy = mr.strategy;
    body.append(h('p', {}, h('b', { class: 'mono' }, mr.source), ' → ', h('b', { class: 'mono' }, mr.target)),
      h('label', { class: 'inline' }, 'strategy ', select(STRATEGIES, strategy, (v) => { strategy = v; })));
    const out = h('div'); body.append(out);
    let close = null;
    renderPreview(out, p, () => strategy, async (resolutions) => {
      const res = await api(`/merge-requests/${mr.id}/approve`, { method: 'POST', body: { strategy, resolutions } }).catch(() => null);
      if (res) { toast('Merge request approved and merged', 'ok'); close?.(); load().then(renderMain); }
    }, null);
    close = modal(mr.title, body, [
      { label: 'Reject', kind: 'danger', onClick: async () => { await api(`/merge-requests/${mr.id}/reject`, { method: 'POST' }); toast('Rejected'); load(); } },
      { label: 'Close' },
    ]);
  }

  // ------------------------------------------------------------ actions
  function forkModal(source, at) {
    const nameIn = h('input', { value: `${source}/${at ? 'at-' + short(at).slice(0, 6) : 'fork-' + Math.random().toString(36).slice(2, 6)}` });
    modal(at ? `Fork from ${short(at)}` : `Fork ${source}`, h('div', { class: 'form' },
      h('p', { class: 'muted' }, at ? 'Time-travel fork: the new branch starts at this historical commit.' : 'O(1) fork: only a pointer is created.'),
      h('label', {}, 'New branch', nameIn)), [
      { label: 'Cancel' },
      { label: 'Fork', kind: 'primary', onClick: async () => {
        const name = nameIn.value.trim();
        const src = branches.find((b) => b.name === source);
        const b = at
          ? await api('/branches', { method: 'POST', body: { name, at, meta: { kind: 'fork', persona: src?.meta?.persona } } })
          : await api('/branch/fork', { method: 'POST', body: { source, name } });
        toast(`Created ${b.name}`, 'ok');
        window.bmNavigate('branches', { name: b.name });
      } },
    ]);
  }

  function rollback(hash) {
    modal('Rollback', h('p', {}, `Create a new commit on ${selected} restoring the state of ${short(hash)}? History is preserved.`), [
      { label: 'Cancel' },
      { label: 'Rollback', kind: 'primary', onClick: async () => {
        const r = await api('/branch/rollback', { method: 'POST', body: { name: selected, commit: hash } });
        toast(`Rolled back (${short(r.commit)})`, 'ok'); selectedCommit = r.commit; renderMain();
      } },
    ]);
  }

  async function abandon() {
    await api('/branch/abandon', { method: 'POST', body: { name: selected } });
    toast('Branch abandoned'); await load(); renderMain();
  }

  function del() {
    modal('Delete branch', h('p', {}, `Delete the ref ${selected}? Commits stay in the object store (content-addressed) but the branch pointer is removed.`), [
      { label: 'Cancel' },
      { label: 'Delete', kind: 'danger', onClick: async () => {
        await api(`/branch?name=${encodeURIComponent(selected)}`, { method: 'DELETE' });
        selected = ''; await load(); renderMain();
      } },
    ]);
  }

  const refresh = debounce(async () => { await load(); }, 500);
  const off = onEvent((ev) => {
    if (['branch_created', 'branch_updated', 'branch_deleted', 'merge', 'merge_request', 'rollback'].includes(ev.kind)) refresh();
  });
  (async () => { await load(); await renderMain(); })();
  return off;
}
