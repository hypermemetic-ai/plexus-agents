<!-- SC-33: the nav (sc-frame's `nav` section; on a phone the whole `#/` screen).
     SUPERCONDUCTOR-27 1, 2, 7: on top "Needs you", across every host, with a count —
     a thread there is not listed again below; then each host (online dot, or dimmed
     with when it was last seen), its threads newest first, grouped by folder when it
     has more than one, the older ones folded. A thread is named by its first message.

     Where the data comes from:
     - `hosts.list`, then `hosts.threads {host}` for each online host — asked again
       every few seconds (plexus-ui's `live` can't read a list answered in one event as
       rows, FRAME-60), at once after a navigation, on `sc.changed` and when the tab
       comes back; a host's rows are replaced only when its answer changed, so nothing
       redraws on a quiet poll. Status is derived from that one answer every time, never
       cached: a thread's undecided requests (`pending`) count only while it runs or is
       parked, so a finished turn drops "Needs you" on the next refresh.
     - A thread's title (its first prompt) and its last activity are its row's
       `title` and `last_activity` in `hosts.threads`, kept by the host as the thread
       advances: nothing is replayed to read them. A host older than those fields
       names a thread by its folder, with no time.
     - Done-unread: a thread idle whose `last_seq` passed the one you last saw it at
       (kept in localStorage; a thread first seen counts as read).

     It says the rows it drew on the sticky topic `sc.nav.threads`, which sc-palette
     reads. Until sc-frame gives the palette its own section, a nav drawn by a page
     carries one itself (when no palette said `sc.palette.here`). -->
<script module>
  export const uses = ["hosts.list", "hosts.threads"];

  const FOLD = 5;          // rows shown per group before "Show N older"
  const QUIET_MS = 8000;   // the poll when nothing moves
  const BUSY_MS = 2500;    // the poll while something works or waits on you

  function load(key) {
    try { return JSON.parse(localStorage.getItem(key) ?? "{}") ?? {}; } catch { return {}; }
  }
  function save(key, value) {
    try { localStorage.setItem(key, JSON.stringify(value)); } catch {}
  }
  const lower = (s) => (typeof s === "string" ? s : Object.keys(s ?? {})[0] ?? "").toLowerCase();
  export const base = (p) => (p ? p.replace(/\/+$/, "").split("/").pop() || p : "");
  export function ago(ms) {
    if (!ms) return "";
    const d = Math.max(0, (Date.now() - ms) / 1000);
    if (d < 60) return "now";
    if (d < 3600) return Math.round(d / 60) + "m";
    if (d < 86400) return Math.round(d / 3600) + "h";
    if (d < 86400 * 14) return Math.round(d / 86400) + "d";
    return new Date(ms).toLocaleDateString(undefined, { month: "short", day: "numeric" });
  }
  function seenWords(secs) {
    if (!secs) return "Offline";
    const d = Math.max(0, Date.now() / 1000 - secs);
    if (d < 90) return "Offline, seen just now";
    if (d < 5400) return `Offline, seen ${Math.round(d / 60)} minutes ago`;
    if (d < 129600) { const h = Math.round(d / 3600); return `Offline, seen ${h} hour${h === 1 ? "" : "s"} ago`; }
    const n = Math.round(d / 86400);
    return `Offline, seen ${n} day${n === 1 ? "" : "s"} ago`;
  }
  const tidy = (text) => {
    const t = String(text ?? "").replace(/\s+/g, " ").trim();
    return t.length > 90 ? t.slice(0, 88).trimEnd() + "…" : t;
  };
</script>

<script>
  import { untrack } from "svelte";
  import { bus } from "plexus-ui/host";
  import Pill from "./sc-pill.svelte";
  import Empty from "./sc-empty.svelte";
  import Palette from "./sc-palette.svelte";

  const b = bus();

  let hosts = $state([]);            // [{ name, online, last_seen }]
  let byHost = $state({});           // host → [ThreadView], newest first
  let loaded = $state(false);
  let refused = $state("");
  let hash = $state(location.hash || "#/");
  let seen = $state(load("sc.nav.seen"));       // thread id → last_seq when last read
  let open = $state({});             // group key → unfolded
  let cursor = $state(-1);           // the keyboard's row in `flat`
  let now = $state(Date.now());      // ticks the relative times

  // ── reading ────────────────────────────────────────────────────────────────
  const first = (r, type) => (r.ok ? (r.events ?? []).find((e) => e.type === type) : null);
  let busy = false, again = false, timer = 0;
  async function refresh() {
    if (busy) { again = true; return; }
    busy = true;
    try {
      const r = await b.call("hosts.list", {});
      const listed = first(r, "hosts");
      if (!listed) { refused = r.message ?? "Your machines could not be listed."; return; }
      refused = "";
      const next = listed.hosts.map((h) => ({ name: h.name, online: !!h.online, last_seen: h.last_seen ?? null }));
      if (JSON.stringify(next) !== JSON.stringify(hosts)) hosts = next;
      await Promise.all(next.filter((h) => h.online).map(async (h) => {
        const ts = first(await b.call("hosts.threads", { host: h.name }), "threads");
        if (!ts) return;   // kept as it was; asked again next time
        const rows = ts.threads.slice().sort((x, y) => (y.last_seq ?? 0) - (x.last_seq ?? 0));
        if (JSON.stringify(rows) !== JSON.stringify(untrack(() => byHost[h.name]))) byHost[h.name] = rows;
      }));
      for (const name of Object.keys(untrack(() => byHost))) {
        if (!next.some((h) => h.name === name)) delete byHost[name];   // unpaired
      }
      loaded = true;
      settle();
    } finally {
      busy = false;
      now = Date.now();
      if (again) { again = false; refresh(); } else schedule();
    }
  }
  function schedule() {
    clearTimeout(timer);
    const moving = untrack(() => all.some((r) => r.status === "working" || r.status === "needs" || r.status === "waiting"));
    timer = setTimeout(() => { if (document.visibilityState !== "hidden") refresh(); else schedule(); }, moving ? BUSY_MS : QUIET_MS);
  }
  const soon = () => setTimeout(refresh, 60);

  $effect(() => {
    refresh();
    const moved = () => { hash = location.hash || "#/"; cursor = -1; soon(); };
    const back = () => { if (document.visibilityState === "visible") refresh(); };
    addEventListener("hashchange", moved);
    document.addEventListener("visibilitychange", back);
    const off = b.on("sc.changed", soon);
    return () => { clearTimeout(timer); removeEventListener("hashchange", moved); document.removeEventListener("visibilitychange", back); off(); };
  });

  function settle() {
    const ids = new Set();
    for (const h of hosts) {
      for (const t of byHost[h.name] ?? []) {
        ids.add(t.id);
        // Seen at first sight: only what moves while you look elsewhere is unread.
        if (seen[t.id] == null) seen[t.id] = t.last_seq ?? 0;
      }
    }
    // Forget what no host lists any more.
    if (Object.keys(seen).some((id) => !ids.has(id)) && loaded && hosts.every((h) => h.online)) {
      for (const id of Object.keys(seen)) if (!ids.has(id)) delete seen[id];
    }
    save("sc.nav.seen", seen);
    // What the probes of an older nav kept, now in the rows.
    try { localStorage.removeItem("sc.nav.titles"); localStorage.removeItem("sc.nav.times"); } catch {}
  }

  // ── status: one per thread, from this refresh's answer alone ──────────────
  function pendingOf(t) {
    const s = lower(t.status);
    // A request still marked undecided on a thread with no turn going is stale.
    return s === "running" || s === "parked" ? (t.pending ?? []) : [];
  }
  function statusOf(h, t) {
    if (!h.online) return "offline";
    if (pendingOf(t).length) return "needs";
    const s = lower(t.status);
    if (s === "running") return "working";
    if (s === "parked") return "waiting";
    if (s === "broken") return "failed";
    if ((t.last_seq ?? 0) > (seen[t.id] ?? 0)) return "done";
    return "idle";
  }

  // ── the rows ──────────────────────────────────────────────────────────────
  const href = (host, id) => `#/h/${encodeURIComponent(host)}/t/${encodeURIComponent(id)}`;
  const hostHref = (host) => `#/h/${encodeURIComponent(host)}`;
  const all = $derived(hosts.flatMap((h) => (byHost[h.name] ?? []).map((t) => {
    const folder = base(t.cwd);
    return {
      key: h.name + "/" + t.id,
      host: h.name,
      id: t.id,
      cwd: t.cwd ?? "",
      folder,
      title: t.title ? tidy(t.title) : folder ? `Thread in ${folder}` : "Untitled thread",
      named: !!t.title,
      at: t.last_activity ?? 0,
      seq: t.last_seq ?? 0,
      status: statusOf(h, t),
      href: href(h.name, t.id),
    };
  })));
  const needs = $derived(all.filter((r) => r.status === "needs").sort((x, y) => y.at - x.at || y.seq - x.seq));
  // Per host: its folders (one group when it has one), each newest first, folded.
  // Online hosts first; each list keeps hosts.list's order.
  const groups = $derived([...hosts.filter((h) => h.online), ...hosts.filter((h) => !h.online)].map((h) => {
    const rows = all.filter((r) => r.host === h.name && r.status !== "needs");
    const folders = [...new Set(rows.map((r) => r.cwd))];
    const split = folders.length > 1;
    const parts = (split ? folders : [null]).map((cwd) => {
      const mine = split ? rows.filter((r) => r.cwd === cwd) : rows;
      const key = h.name + "|" + (cwd ?? "");
      const unfolded = !!open[key];
      return { key, cwd, label: cwd ? base(cwd) : "No folder", rows: unfolded ? mine : mine.slice(0, FOLD), hidden: mine.length - FOLD, unfolded };
    });
    return { ...h, split, parts, count: rows.length };
  }));
  const flat = $derived([...needs, ...groups.flatMap((g) => g.parts.flatMap((p) => p.rows))]);
  const selected = $derived(flat.find((r) => r.href === hash)?.key ?? "");

  // Reading a thread marks it read.
  $effect(() => {
    const r = all.find((r) => r.href === hash);
    if (r && seen[r.id] !== r.seq) untrack(() => { seen[r.id] = r.seq; save("sc.nav.seen", seen); });
  });
  // What the palette searches.
  $effect(() => {
    const rows = all.map(({ host, id, title, folder, status, href, at }) => ({ host, id, title, folder, status, href, at }));
    const hs = hosts.map(({ name, online }) => ({ name, online }));
    untrack(() => b.emit("sc.nav.threads", { rows, hosts: hs }, { sticky: true }));
  });
  $effect(() => { const t = setInterval(() => (now = Date.now()), 30000); return () => clearInterval(t); });
  const when = (r) => (now, ago(r.at));

  // ── keys ──────────────────────────────────────────────────────────────────
  function move(by) {
    if (!flat.length) return;
    const from = cursor >= 0 ? cursor : Math.max(-1, flat.findIndex((r) => r.key === selected));
    cursor = Math.min(flat.length - 1, Math.max(0, from < 0 && by < 0 ? 0 : from + by));
    queueMicrotask(() => document.querySelector(`.sc-nav [data-row="${CSS.escape(flat[cursor]?.key ?? "")}"]`)?.scrollIntoView({ block: "nearest" }));
  }
  function go(i) {
    const r = flat[i];
    if (r) { cursor = -1; b.navigate(r.href); }
  }
  const jumps = {};
  for (let n = 1; n <= 9; n++) {
    jumps["ctrl+" + n] = { to: () => go(n - 1), label: `Open thread ${n} in the list` };
    jumps["meta+" + n] = { to: () => go(n - 1), label: `Open thread ${n} in the list` };
  }
  b.keys({
    j: { to: () => move(1), label: "Next thread" },
    k: { to: () => move(-1), label: "Previous thread" },
    ...jumps,
  });
  // Enter is bound only while the keyboard has a row, so it is the page's otherwise.
  $effect(() => {
    if (cursor < 0) return;
    return b.keys({ enter: { to: () => go(cursor), label: "Open the thread" } });
  });

  // ── the palette, until sc-frame gives it a section ───────────────────────
  let framed = $state(!!b.last("sc.palette.here"));
  let embed = $state(false);
  b.on("sc.palette.here", () => (framed = true));
  $effect(() => { const t = setTimeout(() => (embed = true), 400); return () => clearTimeout(t); });
  let opener = $state(0);
  function search() {
    if (framed) b.emit("sc.palette.open", {});
    else opener += 1;
  }
  const isMac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
  const server = location.origin.replace(/^http/, "ws") + "/ws";
  let copied = $state(false);
  function copy(text) {
    navigator.clipboard?.writeText(text).then(() => { copied = true; setTimeout(() => (copied = false), 1600); }, () => {});
  }
</script>

{#snippet row(r, showHost)}
  <a class="row" href={r.href} data-row={r.key} data-status={r.status}
     aria-current={r.key === selected ? "page" : undefined}
     data-cursor={flat[cursor]?.key === r.key ? "" : undefined}
     onclick={() => (cursor = -1)}>
    <span class="line">
      <span class="title" class:unnamed={!r.named} title={r.title}>{r.title}</span>
      <span class="when">{when(r)}</span>
    </span>
    {#if showHost || r.status !== "idle"}
      <span class="meta">
        {#if r.status !== "idle" && r.status !== "needs"}<Pill status={r.status}/>{/if}
        {#if showHost}<span class="where">{r.folder ? `${r.folder} on ${r.host}` : `on ${r.host}`}</span>{/if}
      </span>
    {/if}
  </a>
{/snippet}

<aside class="sc-nav" data-surface="sc-nav" aria-label="Threads">
  <div class="head">
    <button type="button" class="search" onclick={search} aria-label="Search threads and actions">
      <svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="7" cy="7" r="4.5"/><path d="M10.5 10.5 14 14"/></svg>
      <span>Search</span>
      <kbd>{isMac ? "⌘K" : "Ctrl K"}</kbd>
    </button>
  </div>

  <div class="scroll">
    {#if refused}
      <Empty title="Your machines aren't showing" body={refused}>
        <a class="link" href={"/login?next=" + encodeURIComponent("/" + hash)}>Sign in again</a>
      </Empty>
    {:else if !loaded}
      <div class="loading" aria-label="Loading"><i></i><i></i><i></i></div>
    {:else if !hosts.length}
      <Empty title="Pair a machine to start" body="Threads run Claude Code on your own computers. Run this on the machine you want to use, then open the link it prints.">
        <div class="cmd">
          <code>{`plexus-agents host \\\n  --server ${server} \\\n  --app ${location.origin} \\\n  --name laptop \\\n  --db ~/.plexus/agents-laptop.db`}</code>
          <button type="button" class="copy" onclick={() => copy(`plexus-agents host --server ${server} --app ${location.origin} --name laptop --db ~/.plexus/agents-laptop.db`)}>{copied ? "Copied" : "Copy"}</button>
        </div>
        <a class="link" href="#/pair">Have a pairing code? Enter it</a>
      </Empty>
    {:else}
      {#if needs.length}
        <section class="needs" aria-label="Needs you">
          <h2><span>Needs you</span><span class="count">{needs.length}</span></h2>
          {#each needs as r (r.key)}{@render row(r, true)}{/each}
        </section>
      {/if}

      {#each groups as g (g.name)}
        <section class="host" data-online={g.online} aria-label={g.name}>
          <div class="hosthead">
            <a class="hostname" href={hostHref(g.name)} aria-current={hash === hostHref(g.name) ? "page" : undefined}>
              <span class="dot" data-online={g.online} aria-hidden="true"></span>
              <span class="name">{g.name}</span>
            </a>
            {#if g.online}
              <a class="new" href={hostHref(g.name) + "/new"} aria-label={`New thread on ${g.name}`}>New thread</a>
            {:else}
              <span class="seen">{(now, seenWords(g.last_seen))}</span>
            {/if}
          </div>
          {#each g.parts as p (p.key)}
            {#if g.split}<h3 class="folder" title={p.cwd}>{p.label}</h3>{/if}
            {#each p.rows as r (r.key)}{@render row(r, false)}{/each}
            {#if p.hidden > 0}
              <button type="button" class="more" onclick={() => (open[p.key] = !p.unfolded)}>
                {p.unfolded ? "Show fewer" : `Show ${p.hidden} older`}
              </button>
            {/if}
          {/each}
          {#if g.online && !g.count && !needs.some((r) => r.host === g.name)}
            <p class="none">No threads yet.</p>
          {/if}
        </section>
      {/each}
    {/if}
  </div>

  {#if hosts.length}
    <div class="foot"><a class="pair" href="#/pair">Pair a machine</a></div>
  {/if}

  {#if embed && !framed}<Palette embedded data={{ rows: all, hosts }} {opener}/>{/if}
</aside>

<style>
  .sc-nav { flex: 0 0 272px; width: 100%; min-width: 0; height: 100%; display: flex; flex-direction: column; background: var(--panel); border-right: var(--border) solid var(--line); font-family: var(--font-text); font-size: var(--text-size); line-height: var(--leading); color: var(--ink); }
  .head { padding: var(--s3) var(--s3) var(--s2); }
  .search { display: flex; align-items: center; gap: var(--s2); width: 100%; min-height: 36px; padding: 0 var(--s2) 0 var(--s3); color: var(--muted); background: var(--bg); border: var(--border) solid var(--line); border-radius: var(--radius-md); cursor: pointer; text-align: left; transition: border-color var(--motion-fast) var(--ease); }
  .search:hover { border-color: var(--muted); }
  .search svg { width: 14px; height: 14px; flex: none; fill: none; stroke: currentColor; stroke-width: 1.6; stroke-linecap: round; }
  .search span { flex: 1; }
  kbd { font: 11px/18px var(--font-text); padding: 0 var(--s1); color: var(--muted); border: var(--border) solid var(--line); border-radius: var(--radius-sm); }

  .scroll { flex: 1; min-height: 0; overflow-y: auto; padding: 0 var(--s2) var(--s3); overscroll-behavior: contain; }

  /* The one loud thing: what is waiting on you. */
  .needs { margin: var(--s1) 0 var(--s3); padding: var(--s1) 0 var(--s1); background: var(--st-needs-bg); border-radius: var(--radius-md); box-shadow: inset 3px 0 0 var(--st-needs-fg); }
  .needs h2 { display: flex; align-items: center; gap: var(--s2); margin: var(--s1) var(--s3) var(--s1) var(--s4); font-size: 13px; font-weight: var(--weight-bold); color: var(--st-needs-fg); }
  .count { min-width: 20px; padding: 0 6px; font-size: 12px; line-height: 20px; text-align: center; color: var(--st-needs-bg); background: var(--st-needs-fg); border-radius: var(--radius-pill); font-variant-numeric: tabular-nums; }
  .needs .row { margin: 0 var(--s1) 0 var(--s2); }
  .needs .row:hover { background: var(--card); }
  .needs .title { font-weight: var(--weight-medium); }

  .host { margin-top: var(--s3); }
  .host[data-online=false] { opacity: .62; }
  .hosthead { display: flex; align-items: center; gap: var(--s2); min-height: 32px; padding: 0 var(--s1) 0 0; }
  .hostname { display: flex; align-items: center; gap: var(--s2); flex: 1; min-width: 0; min-height: 32px; padding: 0 var(--s2); color: var(--ink); border-radius: var(--radius-sm); }
  .hostname:hover { background: var(--hover); }
  .hostname[aria-current] { background: var(--sel); }
  .name { font-weight: var(--weight-bold); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .dot { width: 8px; height: 8px; flex: none; border-radius: 50%; background: var(--disabled); }
  .dot[data-online=true] { background: var(--live); }
  .new { flex: none; padding: var(--s1) var(--s2); font-size: 13px; color: var(--accent); border-radius: var(--radius-sm); }
  .new:hover { background: var(--hover); }
  .seen { flex: none; padding-right: var(--s2); font-size: 12px; color: var(--muted); }
  .host .row, .host .more, .none { padding-left: var(--s5); }
  .folder { margin: var(--s2) var(--s2) 2px var(--s5); font-size: 12px; font-weight: var(--weight-medium); color: var(--muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

  .row { display: flex; flex-direction: column; justify-content: center; gap: 2px; min-height: 36px; padding: var(--s1) var(--s2); color: var(--ink); border-radius: var(--radius-sm); transition: background-color var(--motion-fast) var(--ease); }
  .row:hover { background: var(--hover); }
  .row[aria-current] { background: var(--sel); box-shadow: inset 2px 0 0 var(--accent); }
  .row[data-cursor] { outline: 2px solid var(--focus); outline-offset: -2px; }
  .line { display: flex; align-items: baseline; gap: var(--s2); min-width: 0; }
  .title { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .title.unnamed { color: var(--muted); }
  .when { flex: none; font-size: 12px; color: var(--muted); font-variant-numeric: tabular-nums; }
  .meta { display: flex; align-items: center; gap: var(--s2); min-width: 0; font-size: 12px; color: var(--muted); }
  .where { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .row[data-status=done] .title { font-weight: var(--weight-medium); }

  .more { display: block; width: 100%; min-height: 32px; padding: 0 var(--s2); font-size: 13px; text-align: left; color: var(--muted); background: none; border: 0; border-radius: var(--radius-sm); cursor: pointer; }
  .more:hover { color: var(--ink); background: var(--hover); }
  .none { margin: var(--s1) var(--s2); font-size: 13px; color: var(--muted); }

  .foot { padding: var(--s2) var(--s3) var(--s3); border-top: var(--border) solid var(--line); }
  .pair { display: flex; align-items: center; min-height: 32px; padding: 0 var(--s2); font-size: 13px; color: var(--muted); border-radius: var(--radius-sm); }
  .pair:hover { color: var(--ink); background: var(--hover); }

  .cmd { display: flex; align-items: flex-start; gap: var(--s2); margin: 0 0 var(--s3); padding: var(--s2) var(--s2) var(--s2) var(--s3); background: var(--bg); border: var(--border) solid var(--line); border-radius: var(--radius-md); }
  .cmd code { flex: 1; min-width: 0; font: 12px/1.6 var(--font-mono); white-space: pre; overflow-x: auto; }
  .copy { flex: none; padding: 2px var(--s2); font-size: 12px; color: var(--ink); background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius-sm); cursor: pointer; }
  .link { font-size: 13px; }

  .loading { display: flex; flex-direction: column; gap: var(--s3); padding: var(--s3) var(--s2); }
  .loading i { display: block; height: 12px; border-radius: var(--radius-sm); background: var(--hover); }
  .loading i:nth-child(2) { width: 70%; }
  .loading i:nth-child(3) { width: 85%; }

  a, button { text-decoration: none; }
  a:focus-visible, button:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }

  @media (max-width: 719px) {
    .sc-nav { border-right: 0; font-size: 15px; }
    .head { padding: var(--s3) var(--s4) var(--s2); }
    .search { min-height: 44px; }
    .search kbd { display: none; }
    .scroll { padding: 0 var(--s2) var(--s4); }
    .row { min-height: 52px; padding: var(--s2) var(--s3); }
    .hosthead, .hostname { min-height: 44px; }
    .new, .pair, .more { min-height: 44px; display: flex; align-items: center; }
    .foot { padding: var(--s1) var(--s4) var(--s3); }
  }
</style>
