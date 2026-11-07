<!-- SC-33: the palette (sc-frame's `palette` section). SUPERCONDUCTOR-27 7: Cmd/Ctrl+K
     opens a search over the threads on every host — by title, folder and host — and
     the actions: a new thread on each online host, pairing a machine. The arrows move
     `[data-selected]`, Enter opens it, Escape closes; on a phone it is a full-width
     sheet from the top, opened by the nav's search button (`sc.palette.open`).
     Modelled on menger's jump-palette-sv: the open palette's keys are bound only
     while it is open.

     It asks nothing itself: the rows are the nav's, said on the sticky topic
     `sc.nav.threads` (or handed as `data` when the nav carries it, `embedded`). -->
<script module>
  export const uses = [];

  // Every word of the query must be found; the best place it was found ranks it.
  function score(row, words) {
    let total = 0;
    for (const w of words) {
      const at = [row.title, row.folder, row.host].map((s) => String(s ?? "").toLowerCase().indexOf(w));
      if (at.every((i) => i < 0)) return -1;
      const field = at.findIndex((i) => i >= 0);
      total += field * 10 + (at[field] === 0 ? 0 : 3);
    }
    return total;
  }
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Pill from "./sc-pill.svelte";

  let { embedded = false, data = null, opener = 0 } = $props();
  const b = bus();

  let heard = $state({ rows: [], hosts: [] });
  if (!embedded) {
    b.on("sc.nav.threads", (d) => (heard = d ?? { rows: [], hosts: [] }), { last: true });
    b.emit("sc.palette.here", {}, { sticky: true });
  }
  const source = $derived(embedded ? (data ?? { rows: [], hosts: [] }) : heard);

  let open = $state(false);
  let q = $state("");
  let sel = $state("");
  const words = $derived(q.toLowerCase().split(/\s+/).filter(Boolean));
  const LABEL = { needs: "Needs you", working: "Working", waiting: "Waiting", failed: "Failed", done: "Done" };

  const threads = $derived.by(() => {
    const rows = source.rows ?? [];
    if (!words.length) {
      const needs = rows.filter((r) => r.status === "needs");
      const rest = rows.filter((r) => r.status !== "needs").sort((x, y) => (y.at || 0) - (x.at || 0));
      return [...needs, ...rest].slice(0, 8);
    }
    return rows.map((r) => ({ r, s: score(r, words) })).filter((x) => x.s >= 0)
      .sort((x, y) => x.s - y.s || (y.r.at || 0) - (x.r.at || 0)).slice(0, 30).map((x) => x.r);
  });
  const actions = $derived.by(() => {
    const all = [
      ...(source.hosts ?? []).filter((h) => h.online).map((h) => ({ key: "a:new:" + h.name, text: `New thread on ${h.name}`, href: `#/h/${encodeURIComponent(h.name)}/new` })),
      { key: "a:pair", text: "Pair a machine", href: "#/pair" },
    ];
    if (!words.length) return all;
    return all.filter((a) => words.every((w) => a.text.toLowerCase().includes(w)));
  });
  const order = $derived([...threads.map((r) => "t:" + r.host + "/" + r.id), ...actions.map((a) => a.key)]);
  const current = $derived(order.includes(sel) ? sel : (order[0] ?? ""));

  function show() { open = true; }
  function close() { open = false; q = ""; sel = ""; }
  function step(by) {
    if (!order.length) return;
    const at = Math.max(0, order.indexOf(current));
    sel = order[Math.min(order.length - 1, Math.max(0, at + by))];
    queueMicrotask(() => document.querySelector(`.sc-palette [data-key="${CSS.escape(sel)}"]`)?.scrollIntoView({ block: "nearest" }));
  }
  function choose() {
    const t = threads.find((r) => "t:" + r.host + "/" + r.id === current);
    const a = actions.find((a) => a.key === current);
    const to = t?.href ?? a?.href;
    if (!to) return;
    close();
    b.navigate(to);
  }

  b.keys({
    "ctrl+k": { to: () => (open ? close() : show()), label: "Search threads and actions" },
    "meta+k": { to: () => (open ? close() : show()), label: "Search threads and actions" },
  });
  if (!embedded) b.on("sc.palette.open", show);
  b.on("sc.palette.close", close);
  let last = opener;
  $effect(() => { if (opener !== last) { last = opener; show(); } });
  $effect(() => {
    if (!open) return;
    return b.keys({
      escape: { to: close, in_inputs: true, label: "Close the search" },
      arrowdown: { to: () => step(1), in_inputs: true, label: "Next result" },
      arrowup: { to: () => step(-1), in_inputs: true, label: "Previous result" },
      enter: { to: choose, in_inputs: true, label: "Open the selected result" },
    });
  });
  const focus = (node) => { node.focus(); };
</script>

<div class="sc-palette" data-surface="sc-palette" data-palette>
  {#if open}
    <div class="scrim" onclick={close} aria-hidden="true"></div>
    <div class="sheet" role="dialog" aria-modal="true" aria-label="Search threads and actions">
      <div class="field">
        <svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14"/></svg>
        <input use:focus bind:value={q} oninput={() => (sel = "")} spellcheck="false" autocomplete="off"
               placeholder="Search threads, folders, machines" aria-label="Search"/>
        <button type="button" class="cancel" onclick={close}>Close</button>
      </div>
      <div class="results">
        {#if threads.length}
          <h2>{words.length ? "Threads" : "Recent threads"}</h2>
          <ul>
            {#each threads as r (r.host + "/" + r.id)}
              {@const key = "t:" + r.host + "/" + r.id}
              <li>
                <a class="hit" href={r.href} data-key={key} data-selected={current === key ? "" : undefined}
                   onclick={close} onmousemove={() => (sel = key)}>
                  <span class="title">{r.title}</span>
                  <span class="where">{r.folder ? `${r.folder} on ${r.host}` : `on ${r.host}`}</span>
                  {#if LABEL[r.status]}<Pill status={r.status}/>{/if}
                </a>
              </li>
            {/each}
          </ul>
        {/if}
        {#if actions.length}
          <h2>Actions</h2>
          <ul>
            {#each actions as a (a.key)}
              <li>
                <a class="hit action" href={a.href} data-key={a.key} data-selected={current === a.key ? "" : undefined}
                   onclick={close} onmousemove={() => (sel = a.key)}>
                  <span class="title">{a.text}</span>
                </a>
              </li>
            {/each}
          </ul>
        {/if}
        {#if !threads.length && !actions.length}
          <p class="empty">Nothing matches “{q}”. Search looks at thread titles, folders and machine names.</p>
        {/if}
      </div>
      <div class="foot" aria-hidden="true"><span><kbd>↑</kbd><kbd>↓</kbd> move</span><span><kbd>Enter</kbd> open</span><span><kbd>Esc</kbd> close</span></div>
    </div>
  {/if}
</div>

<style>
  .scrim { position: fixed; inset: 0; z-index: 40; background: var(--ink); opacity: .28; }
  .sheet { position: fixed; z-index: 41; top: 12vh; left: 50%; transform: translateX(-50%); display: flex; flex-direction: column; width: min(600px, calc(100vw - 32px)); max-height: 72vh; overflow: hidden; font-family: var(--font-text); font-size: var(--text-size); line-height: var(--leading); color: var(--ink); background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius-lg); box-shadow: 0 18px 48px -12px var(--line); }
  .field { display: flex; align-items: center; gap: var(--s2); padding: 0 var(--s2) 0 var(--s4); border-bottom: var(--border) solid var(--line); }
  .field svg { width: 16px; height: 16px; flex: none; fill: none; stroke: var(--muted); stroke-width: 1.6; stroke-linecap: round; }
  input { flex: 1; min-width: 0; height: 52px; padding: 0; font-size: 16px; color: var(--ink); background: none; border: 0; outline: none; }
  input::placeholder { color: var(--muted); }
  .cancel { display: none; padding: var(--s2); font-size: 15px; color: var(--accent); background: none; border: 0; cursor: pointer; }
  .results { overflow-y: auto; padding: var(--s1) var(--s2) var(--s2); overscroll-behavior: contain; }
  h2 { margin: var(--s3) var(--s2) var(--s1); font-size: 12px; font-weight: var(--weight-medium); color: var(--muted); }
  ul { list-style: none; margin: 0; padding: 0; }
  .hit { display: flex; align-items: center; gap: var(--s3); min-height: 40px; padding: var(--s1) var(--s3); color: var(--ink); text-decoration: none; border-radius: var(--radius-md); }
  .hit[data-selected] { background: var(--sel); box-shadow: inset 2px 0 0 var(--accent); }
  .hit:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }
  .title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .where { flex: none; max-width: 40%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; color: var(--muted); }
  .action .title { color: var(--accent); }
  .empty { margin: var(--s4) var(--s3); color: var(--muted); }
  .foot { display: flex; gap: var(--s4); padding: var(--s2) var(--s4); font-size: 12px; color: var(--muted); border-top: var(--border) solid var(--line); }
  kbd { display: inline-block; min-width: 18px; margin-right: 2px; padding: 0 var(--s1); font: 11px/17px var(--font-text); text-align: center; color: var(--ink); border: var(--border) solid var(--line); border-bottom-width: 2px; border-radius: var(--radius-sm); }

  @media (max-width: 719px) {
    .sheet { top: 0; left: 0; transform: none; width: 100vw; max-height: 100dvh; height: 100dvh; border: 0; border-radius: 0; box-shadow: none; }
    .field { padding: 0 var(--s2) 0 var(--s4); }
    .cancel { display: block; min-height: 44px; }
    .hit { min-height: 52px; flex-wrap: wrap; row-gap: 0; }
    .title { flex-basis: 100%; }
    .where { max-width: none; flex: 1; }
    .foot { display: none; }
  }
</style>
