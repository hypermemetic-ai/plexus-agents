<!-- SUPERCONDUCTOR-34: `#/`, the desktop landing (on a phone the nav is home).
     What needs you comes first, as links straight to it; then each machine's
     recent threads and a way to start one; with no machines, how to connect the
     first. Signed out, a link to sign in. Renders main content only: sc-frame
     draws the top bar and the nav. -->
<script module>
  export const uses = ["auth.whoami", "hosts.list", "hosts.threads"];
</script>

<script>
  import { bus, live } from "plexus-ui/host";
  import Button from "./sc-button.svelte";
  import Empty from "./sc-empty.svelte";
  import Pill from "./sc-pill.svelte";

  const b = bus();
  const door = live({ snapshot: "auth.whoami", params: {}, key: "principal", on: {} });
  const me = $derived(door.rows.find((r) => r.principal) ?? null);
  let hosts = $state(null);
  let threads = $state({});
  let said = $state("");
  let copied = $state(false);

  const event = (r, type) => (r.events ?? []).find((e) => e.type === type);
  const lower = (s) => (typeof s === "string" ? s : Object.keys(s ?? {})[0] ?? "").toLowerCase();
  function statusOf(t) {
    const s = lower(t.status);
    if ((t.pending ?? []).length || s === "parked") return "needs";
    return s === "running" ? "working" : s === "broken" ? "failed" : "idle";
  }
  const base = (p) => (p ?? "").replace(/\/+$/, "").split("/").pop() || p || "No folder";
  const ago = (secs) => {
    if (!secs) return "never";
    const d = Date.now() / 1000 - secs;
    const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
    if (d < 60) return "just now";
    if (d < 3600) return rtf.format(-Math.round(d / 60), "minute");
    if (d < 86400) return rtf.format(-Math.round(d / 3600), "hour");
    return rtf.format(-Math.round(d / 86400), "day");
  };
  const threadHref = (h, t) => `#/h/${encodeURIComponent(h)}/t/${encodeURIComponent(t)}`;

  async function load() {
    const r = await b.call("hosts.list", {});
    if (!r.ok) { said = r.message ?? "Couldn't load your machines."; hosts = hosts ?? []; return; }
    const list = event(r, "hosts")?.hosts ?? [];
    hosts = list;
    const got = await Promise.all(list.filter((h) => h.online).map(async (h) => {
      const t = await b.call("hosts.threads", { host: h.name });
      const rows = (event(t, "threads")?.threads ?? []).slice().sort((x, y) => (y.last_seq ?? 0) - (x.last_seq ?? 0));
      return [h.name, rows];
    }));
    threads = Object.fromEntries(got);
    said = "";
  }
  $effect(() => {
    if (!me) return;
    load();
    const timer = setInterval(load, 5000);
    return () => clearInterval(timer);
  });

  const online = $derived((hosts ?? []).filter((h) => h.online));
  const needs = $derived(Object.entries(threads).flatMap(([h, ts]) => ts.filter((t) => statusOf(t) === "needs").map((t) => ({ host: h, t }))));
  const command = $derived(`plexus-agents host --server ${location.origin.replace(/^http/, "ws")}/ws --name my-laptop`);
  async function copy() {
    try { await navigator.clipboard.writeText(command); copied = true; setTimeout(() => (copied = false), 1800); } catch { /* no clipboard here */ }
  }
</script>

<div class="sc-page">
  {#if door.status === "pending"}
    <p class="quiet">Loading…</p>
  {:else if !me}
    <Empty title="Sign in to reach your machines" body="Superconductor shows the Claude Code threads running on your own computers, and lets you answer them from here.">
      <p><a class="link-button" href={"/login?next=" + encodeURIComponent("/" + (location.hash || ""))}>Sign in with GitHub</a></p>
    </Empty>
  {:else if hosts === null}
    <p class="quiet">Loading your machines…</p>
  {:else if hosts.length === 0}
    <h1>Connect your first machine</h1>
    <p class="lede">Claude runs on your own computer; this page is how you reach it from anywhere. On the machine you want to use, run:</p>
    <div class="command">
      <code>{command}</code>
      <Button onclick={copy}>{copied ? "Copied" : "Copy"}</Button>
    </div>
    <p class="quiet">Replace <code class="inline">my-laptop</code> with a name you'll recognise. The command prints a link and a short code. Open the link here, check the code matches, and the machine is yours.</p>
    <p><a href="#/pair">I have a code already</a></p>
  {:else}
    {#if said}<p class="error" role="alert">{said}</p>{/if}

    {#if needs.length}
      <section class="needs" aria-labelledby="needs-h">
        <h2 id="needs-h">{needs.length === 1 ? "A thread is waiting for you" : `${needs.length} threads are waiting for you`}</h2>
        {#each needs as n (n.host + n.t.id)}
          <a class="row" href={threadHref(n.host, n.t.id)}>
            <span class="what"><strong>{base(n.t.cwd)}</strong><span class="where">on {n.host}</span></span>
            <span class="go">Answer</span>
          </a>
        {/each}
      </section>
    {/if}

    <header class="head">
      <h1>{needs.length ? "Everything else" : "Recent threads"}</h1>
      {#if online.length}
        <Button kind="primary" onclick={() => b.navigate(`#/h/${encodeURIComponent(online[0].name)}/new`)}>Start a thread</Button>
      {/if}
    </header>

    {#each hosts as h (h.name)}
      {@const ts = (threads[h.name] ?? []).filter((t) => statusOf(t) !== "needs").slice(0, 6)}
      <section class="host">
        <h2><a href={`#/h/${encodeURIComponent(h.name)}`}>{h.name}</a>
          <span class="state" data-online={h.online}>{h.online ? "Online" : `Offline, last seen ${ago(h.last_seen)}`}</span></h2>
        {#if !h.online}
          <p class="quiet">Its threads show up here when it's back.</p>
        {:else if ts.length === 0}
          <p class="quiet">No threads yet. <a href={`#/h/${encodeURIComponent(h.name)}/new`}>Start one on {h.name}</a></p>
        {:else}
          {#each ts as t (t.id)}
            <a class="row" href={threadHref(h.name, t.id)}>
              <span class="what"><strong>{base(t.cwd)}</strong><span class="path">{t.cwd ?? ""}</span></span>
              <Pill status={statusOf(t)}/>
            </a>
          {/each}
        {/if}
      </section>
    {/each}
  {/if}
</div>

<style>
  .sc-page { max-width: 760px; padding: var(--s6) var(--s5); }
  h1 { margin: 0; font-size: 22px; font-weight: 600; letter-spacing: -0.01em; }
  h2 { margin: 0 0 var(--s2); font-size: 15px; font-weight: 600; }
  p { margin: var(--s3) 0; }
  .lede { max-width: 62ch; color: var(--ink); }
  .quiet { color: var(--muted); }
  .error { color: var(--st-failed-fg); }
  .head { display: flex; align-items: center; justify-content: space-between; gap: var(--s3); margin: var(--s6) 0 var(--s4); }
  .head:first-child { margin-top: 0; }

  .needs { padding: var(--s4); border-left: 4px solid var(--st-needs-fg); background: var(--st-needs-bg); border-radius: var(--radius-sm); }
  .needs h2 { color: var(--st-needs-fg); }
  .needs .row { border-color: var(--st-needs-bg); }
  .needs .row:hover { background: var(--card); }
  .go { color: var(--st-needs-fg); font-weight: 600; }

  .host { margin-top: var(--s5); }
  .host h2 { display: flex; align-items: baseline; flex-wrap: wrap; gap: var(--s3); }
  .host h2 a { color: var(--ink); }
  .state { font-size: 13px; font-weight: 400; color: var(--muted); }
  .state[data-online=true] { color: var(--live); }

  .row { display: flex; align-items: center; gap: var(--s3); min-height: 48px; padding: var(--s2) var(--s3); color: var(--ink); border-top: 1px solid var(--line); }
  .row:hover { background: var(--hover); }
  .row:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }
  .what { flex: 1; min-width: 0; display: flex; align-items: baseline; gap: var(--s3); }
  .what strong { font-weight: 500; white-space: nowrap; }
  .path, .where { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); font-size: 13px; }
  .path { font-family: var(--font-mono); font-size: 12px; }

  .command { display: flex; align-items: center; gap: var(--s3); margin: var(--s4) 0; padding: var(--s3) var(--s3) var(--s3) var(--s4); background: var(--panel); border: 1px solid var(--line); border-radius: var(--radius-md); }
  .command code { flex: 1; min-width: 0; font-family: var(--font-mono); font-size: 13px; overflow-wrap: anywhere; }
  code.inline { font-family: var(--font-mono); font-size: 13px; }
  .link-button { display: inline-flex; align-items: center; min-height: 44px; padding: 0 var(--s4); color: var(--on-accent); background: var(--accent); border-radius: var(--radius-sm); font-weight: 500; }

  @media (max-width: 719px) {
    .sc-page { padding: var(--s4); }
    .what { flex-direction: column; gap: 0; }
    .command { flex-direction: column; align-items: stretch; }
  }
</style>
