<!-- SUPERCONDUCTOR-34: `#/h/:host`, the machine screen (after Happy's): its name,
     online or when last seen in words, what it is in plain words, its threads
     grouped by folder (most recent first), New thread as the one primary action,
     and unpair as a quiet separate danger action with an inline confirm that says
     what it does. Main content only: sc-frame draws the top bar and the nav.

     superconductor-ui/modes: under the threads, what the host itself allows
     (`hosts.capabilities`): the folders threads start under (its --root) and
     whether "Bypass all permissions" is on (--allow-bypass), with the command
     that changes it. A host from before modes shows neither. -->
<script module>
  export const uses = ["hosts.list", "hosts.threads", "hosts.unpair", "hosts.capabilities"];
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Button from "./sc-button.svelte";
  import Empty from "./sc-empty.svelte";
  import Pill from "./sc-pill.svelte";

  let { host } = $props();
  const b = bus();
  let info = $state(undefined);
  let threads = $state([]);
  let said = $state("");
  let confirming = $state(false);
  let unpairing = $state(false);
  let caps = $state(null);

  const event = (r, type) => (r.events ?? []).find((e) => e.type === type);
  const failure = (r) => event(r, "failed")?.message ?? (r.ok ? "" : (r.message ?? ""));
  const lower = (s) => (typeof s === "string" ? s : Object.keys(s ?? {})[0] ?? "").toLowerCase();
  function statusOf(t) {
    const s = lower(t.status);
    if ((t.pending ?? []).length || s === "parked") return "needs";
    return s === "running" ? "working" : s === "broken" ? "failed" : "idle";
  }
  const NOTE = { needs: "Waiting for your answer", working: "Claude is working", failed: "Stopped and can't continue", idle: "" };
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
  const turns = (n) => `${n} ${n === 1 ? "turn" : "turns"}`;

  async function load() {
    const r = await b.call("hosts.list", {});
    info = (event(r, "hosts")?.hosts ?? []).find((h) => h.name === host) ?? null;
    if (!info?.online) { threads = []; return; }
    const [t, c] = await Promise.all([b.call("hosts.threads", { host }), caps ? null : b.call("hosts.capabilities", { host })]);
    if (c) caps = event(c, "capabilities") ?? null;
    threads = (event(t, "threads")?.threads ?? []).slice().sort((x, y) => (y.last_seq ?? 0) - (x.last_seq ?? 0));
    said = failure(t);
  }
  $effect(() => {
    host;
    info = undefined; confirming = false; said = ""; caps = null;
    load();
    const timer = setInterval(load, 5000);
    return () => clearInterval(timer);
  });

  // Folders in the order of their most recent thread.
  const folders = $derived.by(() => {
    const by = new Map();
    for (const t of threads) {
      const k = t.cwd ?? "";
      if (!by.has(k)) by.set(k, []);
      by.get(k).push(t);
    }
    return [...by.entries()];
  });

  async function unpair() {
    unpairing = true;
    const r = await b.call("hosts.unpair", { name: host });
    unpairing = false;
    if (event(r, "unpaired")) b.navigate("#/"); else said = failure(r) || "The server didn't confirm the unpairing.";
  }
  const home = (p) => (p ?? "").replace(/^\/(Users|home)\/[^/]+/, "~");
  const threadHref = (t) => `#/h/${encodeURIComponent(host)}/t/${encodeURIComponent(t.id)}`;
</script>

<div class="sc-page">
  {#if info === undefined}
    <p class="quiet">Loading {host}…</p>
  {:else if info === null}
    <Empty title={`${host} isn't one of your machines`} body="It may have been unpaired, or the link is mistyped. Your machines are listed on the home page.">
      <p><a href="#/">Go to your machines</a></p>
    </Empty>
  {:else}
    <header class="head">
      <div class="id">
        <h1>{host}</h1>
        <p class="state" data-online={info.online}>
          <span class="dot" aria-hidden="true"></span>
          {info.online ? "Online now" : `Offline, last seen ${ago(info.last_seen)}`}
        </p>
      </div>
      <Button kind="primary" disabled={!info.online} onclick={() => b.navigate(`#/h/${encodeURIComponent(host)}/new`)}>New thread</Button>
    </header>
    <p class="about">
      {lower(info.mode) === "machine"
        ? `A computer you paired. It connects by itself with its own key, and Claude runs there, in folders on ${host}.`
        : `A computer signed in with your own account. Claude runs there, in folders on ${host}.`}
    </p>

    {#if said}<p class="error" role="alert">{said}</p>{/if}

    {#if !info.online}
      <Empty title={`${host} is offline`} body={`Run plexus-agents host on ${host} and it reconnects. Its threads show up here when it does.`}/>
    {:else if threads.length === 0}
      <Empty title="No threads yet" body={`Start one and Claude works in a folder on ${host}.`}>
        <p><Button kind="primary" onclick={() => b.navigate(`#/h/${encodeURIComponent(host)}/new`)}>Start a thread</Button></p>
      </Empty>
    {:else}
      {#each folders as [cwd, ts] (cwd)}
        <section class="folder">
          <h2><span class="name">{base(cwd)}</span><span class="path">{cwd}</span></h2>
          {#each ts as t (t.id)}
            {@const st = statusOf(t)}
            <a class="row" href={threadHref(t)} data-status={st}>
              <Pill status={st}/>
              <span class="note">{NOTE[st] || turns(t.last_turn ?? 0)}</span>
              <span class="turns">{NOTE[st] ? turns(t.last_turn ?? 0) : ""}</span>
            </a>
          {/each}
        </section>
      {/each}
    {/if}

    {#if info.online && caps}
      <section class="caps" aria-labelledby="caps-h">
        <h2 id="caps-h">What {host} allows</h2>
        <dl>
          <dt>Threads start in</dt>
          <dd>
            {#each caps.roots ?? [] as r (r)}<code class="root" title={r}>{home(r)}</code>{/each}
            <span class="quiet">and the folders under {(caps.roots ?? []).length === 1 ? "it" : "them"}. To change this, restart the host with <code>--root DIR</code>.</span>
          </dd>
          <dt>Bypass all permissions</dt>
          {#if caps.bypass}
            <dd><span class="on-bypass">Allowed.</span> <span class="quiet">A thread may run every command without asking. Restart the host without <code>--allow-bypass</code> to turn this off.</span></dd>
          {:else}
            <dd>Off. <span class="quiet">Every thread asks before acting, unless you choose otherwise per thread. To allow it, restart the host with <code>--allow-bypass</code>.</span></dd>
          {/if}
        </dl>
      </section>
    {/if}

    <section class="danger" aria-labelledby="unpair-h">
      <h2 id="unpair-h">Unpair {host}</h2>
      {#if !confirming}
        <p class="quiet">Stop this machine connecting here. You can pair it again later.</p>
        <Button onclick={() => (confirming = true)}>Unpair…</Button>
      {:else}
        <div class="confirm" role="group" aria-label="Confirm unpairing">
          <p>Unpairing makes the server refuse {host}'s key and closes its connection now. Threads already on {host} stay there, but you can't open them here until you pair it again.</p>
          <div class="actions">
            <Button kind="danger" disabled={unpairing} onclick={unpair}>{unpairing ? "Unpairing…" : `Unpair ${host}`}</Button>
            <Button onclick={() => (confirming = false)}>Keep it</Button>
          </div>
        </div>
      {/if}
    </section>
  {/if}
</div>

<style>
  .sc-page { max-width: 760px; padding: var(--s6) var(--s5); }
  h1 { margin: 0; font-size: 26px; font-weight: 600; letter-spacing: -0.015em; overflow-wrap: anywhere; }
  h2 { margin: 0; font-size: 15px; font-weight: 600; }
  p { margin: var(--s3) 0; }
  .quiet { color: var(--muted); }
  .error { color: var(--st-failed-fg); }

  .head { display: flex; align-items: flex-start; justify-content: space-between; gap: var(--s4); }
  .id { min-width: 0; }
  .state { display: flex; align-items: center; gap: var(--s2); margin: var(--s1) 0 0; color: var(--muted); }
  .dot { width: 8px; height: 8px; border-radius: var(--radius-pill); background: var(--disabled); }
  .state[data-online=true] { color: var(--live); }
  .state[data-online=true] .dot { background: var(--live); }
  .about { max-width: 62ch; margin: var(--s4) 0 var(--s6); color: var(--muted); }

  .folder { margin-bottom: var(--s5); }
  .folder h2 { display: flex; align-items: baseline; gap: var(--s3); padding-bottom: var(--s2); }
  .folder .name { white-space: nowrap; }
  .folder .path { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font: 400 12px var(--font-mono); color: var(--muted); }
  .row { display: flex; align-items: center; gap: var(--s3); min-height: 48px; padding: var(--s2) var(--s3); color: var(--ink); border-top: 1px solid var(--line); }
  .row:last-child { border-bottom: 1px solid var(--line); }
  .row:hover { background: var(--hover); }
  .row:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }
  .row[data-status=needs] .note { color: var(--st-needs-fg); font-weight: 500; }
  .note { flex: 1; min-width: 0; }
  .turns { color: var(--muted); font-size: 13px; white-space: nowrap; }

  .caps { margin-top: var(--s6); padding-top: var(--s5); border-top: 1px solid var(--line); }
  dl { display: grid; grid-template-columns: max-content minmax(0, 1fr); gap: var(--s3) var(--s5); margin: var(--s4) 0 0; }
  dt { font-weight: 500; }
  dd { margin: 0; max-width: 62ch; line-height: var(--leading); overflow-wrap: anywhere; }
  dd code.root { display: inline-block; margin-right: var(--s2); font: 13px var(--font-mono); color: var(--ink); }
  .quiet code { font: 12.5px var(--font-mono); color: var(--ink); }
  .on-bypass { color: var(--danger); font-weight: 500; }
  .danger { margin-top: var(--s6); padding-top: var(--s5); border-top: 1px solid var(--line); }
  .confirm { padding: var(--s4); border: 1px solid var(--st-failed-bg); background: var(--panel); border-radius: var(--radius-md); }
  .confirm p { margin-top: 0; max-width: 62ch; }
  .actions { display: flex; flex-wrap: wrap; gap: var(--s3); }

  @media (max-width: 719px) {
    .sc-page { padding: var(--s4); }
    .head { flex-direction: column; align-items: stretch; }
    .folder h2 { flex-direction: column; align-items: stretch; gap: 0; }
    .folder .path { white-space: normal; overflow-wrap: anywhere; }
    .actions { flex-direction: column; }
    dl { grid-template-columns: minmax(0, 1fr); gap: var(--s1); }
    dd + dt { margin-top: var(--s3); }
  }
</style>
