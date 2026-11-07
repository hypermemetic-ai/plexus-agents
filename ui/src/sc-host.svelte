<!-- SUPERCONDUCTOR-11: `#/h/:host` — a host's page (SUPERCONDUCTOR-27's 8, after
     Happy's machine screen): online or last seen, how it signs in, its threads with
     one status each, a new thread in a folder there (the folder remembered per host
     in this browser), and unpair behind a confirm. -->
<script module>
  export const uses = ["hosts.list", "hosts.threads", "hosts.pending", "hosts.start", "hosts.unpair"];
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Top from "./sc-top.svelte";
  import Nav from "./sc-nav.svelte";
  import Pill from "./sc-pill.svelte";

  let { host } = $props();
  const b = bus();
  let info = $state(null);
  let threads = $state([]);
  let pending = $state([]);
  let said = $state("");
  let starting = $state(false);
  const key = $derived("superconductor.cwd." + host);
  const remembered = (k) => { try { return localStorage.getItem(k) || ""; } catch { return ""; } };
  let cwd = $state("");
  let prompt = $state("");

  const event = (r, type) => (r.events ?? []).find((e) => e.type === type);
  const failure = (r) => event(r, "failed")?.message ?? (r.ok ? "" : (r.message ?? ""));
  async function load() {
    const r = await b.call("hosts.list", {});
    info = (event(r, "hosts")?.hosts ?? []).find((h) => h.name === host) ?? null;
    if (!info?.online) { threads = []; pending = []; return; }
    const [t, p] = await Promise.all([b.call("hosts.threads", { host }), b.call("hosts.pending", { host })]);
    threads = (event(t, "threads")?.threads ?? []).slice().reverse();
    pending = event(p, "pending")?.requests ?? [];
    said = failure(t);
  }
  $effect(() => {
    cwd = remembered(key);
    load();
    const timer = setInterval(load, 4000);
    return () => clearInterval(timer);
  });

  const lower = (s) => (typeof s === "string" ? s : Object.keys(s ?? {})[0] ?? "").toLowerCase();
  function statusOf(t) {
    if (pending.some((r) => r.thread === t.id) || (t.pending ?? []).length) return "needs";
    const s = lower(t.status);
    return s === "running" ? "working" : s === "broken" ? "failed" : "idle";
  }
  async function start(ev) {
    ev.preventDefault();
    if (!cwd.trim() || !prompt.trim()) return;
    starting = true;
    try { localStorage.setItem(key, cwd.trim()); } catch { /* a private window */ }
    const r = await b.call("hosts.start", { host, cwd: cwd.trim(), prompt });
    starting = false;
    const started = event(r, "started");
    if (started) {
      prompt = "";
      b.navigate(`#/h/${encodeURIComponent(host)}/t/${encodeURIComponent(started.thread)}`);
    } else said = failure(r) || "the host answered nothing";
  }
  async function unpair() {
    if (!confirm(`Unpair ${host}? Its key is refused from now on, and its socket closed.`)) return;
    const r = await b.call("hosts.unpair", { name: host });
    if (event(r, "unpaired")) b.navigate("#/"); else said = failure(r);
  }
  function enter(ev) {
    if (ev.key === "Enter" && (ev.metaKey || ev.ctrlKey)) { ev.preventDefault(); ev.currentTarget.form.requestSubmit(); }
  }
  const when = (secs) => (secs ? new Date(secs * 1000).toLocaleString() : "never");
</script>

<div class="page">
  <Top/>
  <div class="body">
    <Nav/>
    <main>
      <div class="head">
        <h1><span class="dot" data-online={info?.online ?? false}></span>{host}</h1>
        <span class="muted">{info ? (info.online ? "online" : "offline, last seen " + when(info.last_seen)) : "not one of your hosts"}{info ? " · signs in as " + info.mode : ""}</span>
        {#if info}<button class="quiet" type="button" onclick={unpair}>Unpair</button>{/if}
      </div>
      {#if said}<p class="error">{said}</p>{/if}
      {#if info?.online}
        <form class="new" onsubmit={start}>
          <label>Folder on {host}<input value={cwd} oninput={(e) => (cwd = e.currentTarget.value)} required placeholder="/Users/you/project" spellcheck="false"/></label>
          <label>Message<textarea rows="3" value={prompt} oninput={(e) => (prompt = e.currentTarget.value)} onkeydown={enter} required placeholder="What should Claude do? (⌘/Ctrl+Enter starts)"></textarea></label>
          <p><button class="primary" type="submit" disabled={starting}>Start a thread</button> <span class="muted">Claude runs on {host}, in that folder, with Bash.</span></p>
        </form>
        <h2>Threads</h2>
        {#each threads as t (t.id)}
          <a class="row" href={`#/h/${encodeURIComponent(host)}/t/${encodeURIComponent(t.id)}`}>
            <Pill status={statusOf(t)}/><code>{String(t.id).slice(0, 8)}</code><span class="cwd">{t.cwd ?? ""}</span>
            <span class="muted">{t.last_turn} turn{t.last_turn === 1 ? "" : "s"}</span>
          </a>
        {:else}
          <p class="muted">No threads yet.</p>
        {/each}
      {:else if info}
        <p class="muted">Start <code>plexus-agents host</code> on {host} and it comes back here.</p>
      {/if}
    </main>
  </div>
</div>

<style>
  .page { display: grid; grid-template-rows: auto 1fr; height: 100vh; }
  .body { display: flex; min-height: 0; }
  main { flex: 1; overflow-y: auto; padding: 24px 32px; max-width: 900px; }
  .head { display: flex; align-items: baseline; gap: 14px; flex-wrap: wrap; }
  h1 { font-size: 20px; margin: 0 0 16px; display: flex; align-items: center; gap: 8px; }
  h2 { font-size: 14px; margin: 24px 0 8px; color: var(--muted); }
  .dot { width: 10px; height: 10px; border-radius: 50%; background: var(--disabled); }
  .dot[data-online=true] { background: var(--live); }
  .new { display: grid; gap: 10px; padding: 14px; background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius-lg); }
  label { display: grid; gap: 4px; color: var(--muted); font-size: 12px; }
  input, textarea { padding: 6px 10px; color: var(--ink); font-size: 14px; background: var(--bg); border: var(--border) solid var(--line); border-radius: var(--radius); }
  input { font-family: var(--font-text); }
  input:focus-visible, textarea:focus-visible { outline: none; border-color: var(--focus); }
  .row { display: flex; align-items: center; gap: 10px; min-height: var(--pitch); padding: 4px 8px; color: var(--ink); border-bottom: var(--border) solid var(--line); }
  .row:hover { background: var(--hover); }
  .cwd { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); font-family: var(--font-text); font-size: 12px; }
  code { font-family: var(--font-text); font-size: 12px; }
  .muted { color: var(--muted); }
  .error { color: var(--warn); }
  .primary { padding: 6px 16px; color: var(--on-accent); background: var(--accent); border: none; border-radius: var(--radius); cursor: pointer; }
  .primary:disabled { background: var(--disabled); }
  .quiet { margin-left: auto; padding: 3px 10px; background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius); cursor: pointer; }
  .quiet:hover { color: var(--warn); border-color: var(--warn); }
  @media (max-width: 700px) { .body :global(.sc-nav) { display: none; } main { padding: 16px; } }
</style>
