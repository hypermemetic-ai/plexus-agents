<!-- SUPERCONDUCTOR-11: the sidebar (SUPERCONDUCTOR-27's first cut, 2): "Needs you"
     pinned on top, across every host; below it each host as a group — an online
     dot, its name, when it was last seen — with its threads, each with one status
     pill, and "+ new thread". Hosts and their threads are asked again every few
     seconds: `hosts.list` and a host's `hosts.threads` / `hosts.pending` answer a
     list in one event, which a `live` collection can't read as rows (a gap of
     plexus-ui's, in the report), so this calls them. -->
<script module>
  export const uses = ["hosts.list", "hosts.threads", "hosts.pending"];
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Pill from "./sc-pill.svelte";

  const b = bus();
  const EVERY_MS = 4000;
  let hosts = $state([]);
  let threads = $state({});
  let pending = $state({});
  let refused = $state("");
  let hash = $state(location.hash || "#/");

  const first = (r, type) => (r.ok ? (r.events ?? []).find((e) => e.type === type) : null);
  async function refresh() {
    const r = await b.call("hosts.list", {});
    const listed = first(r, "hosts");
    if (!listed) { refused = r.ok ? "" : (r.message ?? ""); hosts = []; return; }
    refused = "";
    hosts = listed.hosts;
    for (const h of hosts.filter((h) => h.online)) {
      const [t, p] = await Promise.all([b.call("hosts.threads", { host: h.name }), b.call("hosts.pending", { host: h.name })]);
      const ts = first(t, "threads");
      const ps = first(p, "pending");
      if (ts) threads[h.name] = ts.threads.slice().reverse();
      if (ps) pending[h.name] = ps.requests;
    }
  }
  $effect(() => {
    refresh();
    const timer = setInterval(refresh, EVERY_MS);
    const moved = () => (hash = location.hash || "#/");
    addEventListener("hashchange", moved);
    return () => { clearInterval(timer); removeEventListener("hashchange", moved); };
  });

  const lower = (s) => (typeof s === "string" ? s : Object.keys(s ?? {})[0] ?? "").toLowerCase();
  function statusOf(host, t) {
    if ((pending[host] ?? []).some((r) => r.thread === t.id) || (t.pending ?? []).length) return "needs";
    const s = lower(t.status);
    if (s === "running") return "working";
    if (s === "broken") return "failed";
    return "idle";
  }
  const base = (p) => (p ? p.replace(/\/+$/, "").split("/").pop() || p : "");
  const short = (id) => String(id).slice(0, 8);
  function ago(secs) {
    if (!secs) return "never";
    const d = Math.max(0, Date.now() / 1000 - secs);
    if (d < 90) return "just now";
    if (d < 5400) return Math.round(d / 60) + "m ago";
    if (d < 129600) return Math.round(d / 3600) + "h ago";
    return Math.round(d / 86400) + "d ago";
  }
  const needs = $derived(
    hosts.flatMap((h) => (threads[h.name] ?? []).filter((t) => statusOf(h.name, t) === "needs").map((t) => ({ host: h.name, t }))),
  );
  const href = (host, t) => `#/h/${encodeURIComponent(host)}/t/${encodeURIComponent(t.id)}`;
</script>

<aside class="sc-nav" data-surface="sc-nav">
  {#if refused}
    <p class="muted">{refused}</p>
  {/if}
  {#if needs.length}
    <section class="needs">
      <h3>Needs you <span class="count">{needs.length}</span></h3>
      {#each needs as n (n.host + n.t.id)}
        <a class="row" href={href(n.host, n.t)} aria-current={hash === href(n.host, n.t) ? "page" : undefined}>
          <Pill status="needs"/><span class="what">{base(n.t.cwd) || short(n.t.id)}</span><span class="muted">{n.host}</span>
        </a>
      {/each}
    </section>
  {/if}
  {#each hosts as h (h.name)}
    <section class="host">
      <a class="head" href={`#/h/${encodeURIComponent(h.name)}`} aria-current={hash === `#/h/${encodeURIComponent(h.name)}` ? "page" : undefined}>
        <span class="dot" data-online={h.online}></span><span class="name">{h.name}</span>
        <span class="muted">{h.online ? "online" : "last seen " + ago(h.last_seen)}</span>
      </a>
      {#if h.online}
        {#each threads[h.name] ?? [] as t (t.id)}
          <a class="row" href={href(h.name, t)} aria-current={hash === href(h.name, t) ? "page" : undefined}>
            <Pill status={statusOf(h.name, t)}/><span class="what" title={t.cwd}>{base(t.cwd) || short(t.id)}</span><span class="muted">{short(t.id)}</span>
          </a>
        {/each}
        <a class="row new" href={`#/h/${encodeURIComponent(h.name)}`}>+ new thread</a>
      {/if}
    </section>
  {:else}
    {#if !refused}<p class="muted">No hosts yet. <a href="#/pair">Pair one</a>.</p>{/if}
  {/each}
</aside>

<style>
  .sc-nav { width: var(--side); flex: none; height: 100%; overflow-y: auto; padding: 10px 8px; background: var(--panel); border-right: var(--border) solid var(--line); }
  h3 { margin: 6px 8px; font-size: 11px; text-transform: uppercase; letter-spacing: .05em; color: var(--muted); }
  .count { color: var(--ink); font-weight: var(--weight-bold); }
  .host { margin-top: 10px; }
  .head, .row { display: flex; align-items: center; gap: 8px; min-height: var(--pitch); padding: 2px 8px; color: var(--ink); border-radius: var(--radius); transition: background-color var(--motion-fast) var(--ease); }
  .row { padding-left: 22px; }
  .head:hover, .row:hover { background: var(--hover); }
  .head[aria-current], .row[aria-current] { background: var(--sel); }
  .name { font-weight: var(--weight-medium); }
  .what { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1; }
  .muted { color: var(--muted); font-size: 12px; }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--disabled); flex: none; }
  .dot[data-online=true] { background: var(--live); }
  .new { color: var(--muted); }
  .needs .row { padding-left: 8px; }
</style>
