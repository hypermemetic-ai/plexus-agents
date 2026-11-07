<!-- SUPERCONDUCTOR-11: `#/h/:host/t/:thread` — a thread as a transcript
     (SUPERCONDUCTOR-27's 4-6): your messages, the agent's text as it streams, each
     tool call a one-line row that expands, a turn's calls folded into one summary
     row once it ends, an approval card inline where the agent stopped, one status
     for the thread, Stop whenever it runs, and the composer: a message sent while
     it runs is queued (in this page) and goes when the turn ends.

     The transcript is a `live` collection keyed by `seq`: `hosts.watch` replays the
     thread and follows it, each `event` an upsert. Its snapshot is
     `hosts.threads { host, thread }`, which answers no row: plexus-ui's `live` has
     no shape for a stream alone (a gap, in the report). Everything the agent says
     is drawn as text, never HTML. -->
<script module>
  export const uses = ["hosts.threads", "hosts.watch", "hosts.pending"];
</script>

<script>
  import { untrack } from "svelte";
  import { bus, live } from "plexus-ui/host";
  import Top from "./sc-top.svelte";
  import Nav from "./sc-nav.svelte";
  import Pill from "./sc-pill.svelte";
  import Tool from "./sc-tool.svelte";
  import Approval from "./sc-approval.svelte";
  import Composer from "./sc-composer.svelte";

  let { host, thread } = $props();
  const b = bus();
  const log = live({
    snapshot: "hosts.threads",
    params: () => ({ host, thread }),
    changes: "hosts.watch",
    changes_params: () => ({ host, thread }),
    key: "seq",
    on: { event: "upsert:event" },
  });

  // The transcript, folded from the events in order.
  let items = $state([]);
  let running = $state(false);
  let failed = $state(false);
  let meta = $state("");
  let asks = $state({});           // request id → its pending record
  let unresolved = new Set();
  let seen = 0;
  let turn = 0;
  let reply = null;
  let tools = new Map();
  let opened = $state({});          // a folded turn's key → expanded

  function reset() {
    items = []; running = false; failed = false; meta = ""; asks = {};
    unresolved = new Set(); seen = 0; turn = 0; reply = null; tools = new Map();
  }
  function push(item) { items.push(item); return items[items.length - 1]; }
  function fold(row) {
    if (row.seq <= seen) return;
    seen = row.seq;
    const ev = row.event ?? {};
    switch (ev.kind) {
      case "turn.started": turn += 1; running = true; failed = false; reply = null; break;
      case "user.message": push({ type: "user", text: ev.text ?? "", turn }); reply = null; break;
      case "session.started": meta = [ev.model, ev.harness_version && "Claude " + ev.harness_version].filter(Boolean).join(" · "); break;
      case "text.delta":
        if (!reply) reply = push({ type: "agent", text: "", turn });
        reply.text += ev.text ?? "";
        break;
      case "message":
        if (reply) reply.text = ev.text ?? reply.text; else push({ type: "agent", text: ev.text ?? "", turn });
        reply = null;
        break;
      case "tool.call": tools.set(ev.id, push({ type: "tool", id: ev.id, name: ev.name, input: ev.input, output: null, is_error: false, done: false, turn })); reply = null; break;
      case "tool.result": { const t = tools.get(ev.id); if (t) { t.output = ev.output; t.is_error = !!ev.is_error; t.done = true; } break; }
      case "input.requested": unresolved.add(ev.id); break;
      case "input.resolved": unresolved.delete(ev.id); if (asks[ev.id]) asks[ev.id] = { ...asks[ev.id], outcome: ev.outcome }; break;
      case "turn.parked": push({ type: "note", text: "Waiting for your answer; the process is parked.", turn }); break;
      case "turn.terminal": {
        running = false; reply = null;
        const st = ev.status && typeof ev.status === "object" ? ev.status : { status: ev.status };
        push({ type: "end", turn, status: st.status, text: st.status === "interrupted" ? "Stopped." : st.status && st.status !== "completed" ? `The turn failed: ${st.failure ?? st.status}${st.detail ? " · " + st.detail : ""}` : "" });
        failed = !!st.status && st.status !== "completed" && st.status !== "interrupted";
        break;
      }
      case "run.ended": running = false; break;
    }
  }
  let asking = false;
  async function ask() {
    const missing = [...unresolved].filter((id) => !asks[id]);
    if (!missing.length || asking) return;
    asking = true;
    const r = await b.call("hosts.pending", { host });
    asking = false;
    const ev = (r.events ?? []).find((e) => e.type === "pending");
    for (const rec of ev?.requests ?? []) {
      if (rec.thread === thread && unresolved.has(rec.request?.id)) asks[rec.request.id] = rec;
    }
  }

  // The collection, as it moves: a whole answer rebuilds, an upsert folds on.
  // Untracked: the fold writes the state this effect would otherwise read.
  $effect(() => untrack(() => {
    return log.subscribe((value, change) => {
      const whole = !change || change.replace;
      if (whole) reset();
      const rows = whole ? value.rows : (change.upsert ?? []);
      const stick = atEnd();
      for (const row of [...rows].sort((a, b) => a.seq - b.seq)) fold(row);
      ask();
      if (stick) follow();
    });
  }));

  // Turns that ended fold their tool calls into one summary row.
  const ended = $derived(new Set(items.filter((i) => i.type === "end").map((i) => i.turn)));
  const blocks = $derived.by(() => {
    const out = [];
    for (const i of items) {
      const last = out[out.length - 1];
      if (i.type === "tool" && ended.has(i.turn)) {
        if (last?.type === "tools" && last.turn === i.turn) last.calls.push(i);
        else out.push({ type: "tools", turn: i.turn, key: "g" + i.id, calls: [i] });
      } else out.push(i);
    }
    return out;
  });
  const status = $derived(Object.values(asks).some((a) => !a.outcome) || unresolved.size ? "needs" : running ? "working" : failed ? "failed" : "idle");

  // Follow the end while it's in view; else offer to jump there.
  let scroller = $state(null);
  let behind = $state(false);
  function atEnd() { return !scroller || scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80; }
  function follow() { requestAnimationFrame(() => { if (scroller) scroller.scrollTop = scroller.scrollHeight; behind = false; }); }
  function scrolled() { behind = !atEnd(); }
</script>

<div class="page">
  <Top/>
  <div class="body">
    <Nav/>
    <main>
      <div class="bar">
        <a href={`#/h/${encodeURIComponent(host)}`}>{host}</a><span class="muted">›</span>
        <code title={thread}>{String(thread).slice(0, 8)}</code>
        <Pill {status}/>
        <span class="muted meta">{meta}</span>
        {#if log.status !== "live" && log.status !== "pending"}<span class="error">{log.message ?? log.status}</span>{/if}
      </div>
      <div class="log" bind:this={scroller} onscroll={scrolled}>
        {#each blocks as blk (blk.key ?? blk)}
          {#if blk.type === "user"}
            <div class="user">{blk.text}</div>
          {:else if blk.type === "agent"}
            <div class="agent">{blk.text}</div>
          {:else if blk.type === "tool"}
            <Tool call={blk}/>
          {:else if blk.type === "tools"}
            <div class="group">
              <button class="summary" type="button" onclick={() => (opened[blk.key] = !opened[blk.key])} aria-expanded={!!opened[blk.key]}>
                {opened[blk.key] ? "▾" : "▸"} {blk.calls.length} tool call{blk.calls.length === 1 ? "" : "s"}{blk.calls.some((c) => c.is_error) ? ", " + blk.calls.filter((c) => c.is_error).length + " failed" : ""}
              </button>
              {#if opened[blk.key]}{#each blk.calls as c (c.id)}<Tool call={c}/>{/each}{/if}
            </div>
          {:else if blk.type === "note"}
            <div class="note">{blk.text}</div>
          {:else if blk.type === "end" && blk.text}
            <div class="note" data-status={blk.status}>{blk.text}</div>
          {/if}
        {/each}
        {#each Object.values(asks) as rec (rec.request.id)}
          <Approval {host} {thread} record={rec}/>
        {/each}
        {#if log.status === "pending"}<p class="muted">…</p>{/if}
      </div>
      {#if behind}<button class="jump" type="button" onclick={follow}>Jump to latest ↓</button>{/if}
      <Composer {host} {thread} {status}/>
    </main>
  </div>
</div>

<style>
  .page { display: grid; grid-template-rows: auto 1fr; height: 100vh; }
  .body { display: flex; min-height: 0; }
  main { position: relative; flex: 1; display: grid; grid-template-rows: auto 1fr auto; min-width: 0; }
  .bar { display: flex; align-items: center; gap: 8px; padding: 8px 20px; border-bottom: var(--border) solid var(--line); background: var(--panel); flex-wrap: wrap; }
  .meta { margin-left: auto; font-size: 12px; }
  .log { overflow-y: auto; padding: 16px 20px 24px; min-height: 0; }
  .user { margin: 14px 0 8px; padding: 8px 12px; white-space: pre-wrap; overflow-wrap: anywhere; background: var(--sel); border-left: 3px solid var(--accent); border-radius: var(--radius); }
  .agent { margin: 6px 0; white-space: pre-wrap; overflow-wrap: anywhere; line-height: 1.55; }
  .group { margin: 2px 0; }
  .summary { padding: 2px 6px; color: var(--muted); font: 12px/1.6 var(--font-text); background: none; border: none; border-radius: var(--radius); cursor: pointer; }
  .summary:hover { background: var(--hover); color: var(--ink); }
  .note { margin: 6px 0; color: var(--muted); font-size: 12px; }
  .note[data-status=failed] { color: var(--warn); }
  .jump { position: absolute; right: 24px; bottom: 120px; padding: 4px 12px; background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius-pill); cursor: pointer; }
  code { font-family: var(--font-text); font-size: 12px; }
  .muted { color: var(--muted); }
  .error { color: var(--warn); font-size: 12px; }
  @media (max-width: 700px) { .body :global(.sc-nav) { display: none; } .log, .composer, .bar { padding-left: 12px; padding-right: 12px; } }
</style>
