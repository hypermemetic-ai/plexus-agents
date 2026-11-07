<!-- SC-31: `#/h/:host/t/:thread` — the thread's main section (the frame draws the
     top bar and the nav): a thin header (the thread's title from its first
     prompt, the host and folder in words, one status pill) and the transcript.
     Transcript, not bubbles (SUPERCONDUCTOR-27): your prompt set apart, Claude's
     prose through sc-text at a reading measure, each run of tool calls one
     sentence (sc-toolrun), the approval card inline where Claude stopped
     (sc-approval), a footer at each turn's end (sc-turnfoot), and the composer
     (sc-composer) at the bottom. What the runtime says to Claude (a resume after
     your answer, a restart) is a small system line, never a message of yours.

     The transcript is a `live` collection keyed by `seq`: `hosts.watch` replays
     the thread and follows it, each `event` an upsert, folded here in order.
     Its snapshot `hosts.threads { host, thread }` answers no row (plexus-ui's
     `live` has no shape for a stream alone); the folder comes from the same
     method, called once. -->
<script module>
  export const uses = ["hosts.threads", "hosts.watch", "hosts.pending"];

  /** What the runtime told Claude, in the operator's words. */
  function runtimeWords(text) {
    if (/stopped while waiting for a decision/.test(text)) return "Picked up again with your answer.";
    if (/isn't known whether it ran/.test(text)) return "Picked up again. An earlier command may or may not have run; Claude was told to check.";
    if (/did finish/.test(text)) return "Picked up again after a restart.";
    return "Picked up again.";
  }

  const OUTCOME = { allowed: "You allowed", denied: "You denied", answered: "You answered", refused: "Refused" };
  /** What an answered request was for: its command, its file, or its tool. */
  function askedFor(rec) {
    const k = rec?.request?.kind ?? {};
    return k.input?.command ?? k.input?.file_path ?? k.tool ?? k.name ?? "";
  }
</script>

<script>
  import { untrack } from "svelte";
  import { bus, live } from "plexus-ui/host";
  import Pill from "./sc-pill.svelte";
  import Text from "./sc-text.svelte";
  import ToolRun from "./sc-toolrun.svelte";
  import TurnFoot from "./sc-turnfoot.svelte";
  import Approval from "./sc-approval.svelte";
  import Composer from "./sc-composer.svelte";
  import Empty from "./sc-empty.svelte";
  import { editOf } from "./sc-diff.svelte";

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

  // The thread's folder, once.
  let folder = $state("");
  $effect(() => {
    const h = host, t = thread;
    untrack(async () => {
      const r = await b.call("hosts.threads", { host: h, thread: t });
      const v = (r.events ?? []).find((e) => e.type === "threads")?.threads?.find((x) => x.id === t);
      if (v?.cwd) folder = v.cwd;
    });
  });

  // The transcript, folded from the events in order.
  let items = $state([]);
  let running = $state(false);
  let thinking = $state(false);
  let failed = $state(false);
  let asks = $state({});           // request id → its pending record, while it waits
  let unresolved = $state(new Set());
  let seen = 0;
  let reply = null;
  let tools = new Map();           // tool_use id → its call (state)
  let turns = new Map();           // turn id → { start, waited, waitFrom, calls }
  let asked = new Map();           // request id → its "ask" item (state)
  let helpers = new Set();         // the top-level helpers started

  function reset() {
    items = []; running = false; thinking = false; failed = false; asks = {}; unresolved = new Set();
    seen = 0; reply = null; tools = new Map(); turns = new Map(); asked = new Map(); helpers = new Set();
  }
  function push(item) { items.push(item); return items[items.length - 1]; }
  function note(text, tone = "quiet") { reply = null; return push({ type: "note", key: "n" + seen, text, tone }); }
  function turnOf(ev) {
    const id = ev.turn ?? ev.in_turn ?? "-";
    if (!turns.has(id)) turns.set(id, { start: ev.at_ms ?? 0, waited: 0, waitFrom: null, calls: [] });
    return turns.get(id);
  }

  function fold(row) {
    if (row.seq <= seen) return;
    seen = row.seq;
    const ev = row.event ?? {};
    // A helper's own steps stay in the helper; only its start and end are drawn.
    if (ev.agent && !/^subagent\./.test(ev.kind)) return;
    switch (ev.kind) {
      case "turn.started": turnOf(ev); running = true; failed = false; reply = null; break;
      case "user.message": {
        const text = ev.text ?? "";
        if (/^\[runtime\]/.test(text)) note(runtimeWords(text));
        else push({ type: "user", key: "u" + seen, text });
        reply = null;
        break;
      }
      case "thinking.delta": thinking = true; break;
      case "text.delta":
        thinking = false;
        if (!reply) reply = push({ type: "agent", key: "t" + seen, text: "" });
        reply.text += ev.text ?? "";
        break;
      case "message":
        thinking = false;
        if (reply) reply.text = ev.text ?? reply.text;
        else if (ev.text) push({ type: "agent", key: "t" + seen, text: ev.text });
        reply = null;
        break;
      case "tool.call": {
        thinking = false;
        reply = null;
        const last = items[items.length - 1];
        const run = last?.type === "run" ? last : push({ type: "run", key: "r" + seen, calls: [] });
        // A call the park stopped is made again on resume: the first goes.
        for (const c of tools.values()) {
          if (c.parked && !c.hidden && c.name === ev.name && JSON.stringify(c.input) === JSON.stringify(ev.input)) c.hidden = true;
        }
        run.calls.push({ id: ev.id, name: ev.name, input: ev.input, output: null, is_error: false, done: false, parked: false, hidden: false });
        const call = run.calls[run.calls.length - 1];
        tools.set(ev.id, call);
        turnOf(ev).calls.push(call);
        break;
      }
      case "tool.result": {
        const c = tools.get(ev.id);
        if (c) { c.output = ev.output; c.is_error = !!ev.is_error; c.done = true; }
        break;
      }
      case "input.requested": {
        unresolved.add(ev.id); unresolved = new Set(unresolved);
        const t = turnOf(ev); t.waitFrom = ev.at_ms ?? null;
        reply = null;
        asked.set(ev.id, push({ type: "ask", key: "a" + ev.id, id: ev.id, outcome: null, record: null }));
        break;
      }
      case "input.resolved": {
        unresolved.delete(ev.id); unresolved = new Set(unresolved);
        const t = turnOf(ev);
        if (t.waitFrom != null && ev.at_ms) { t.waited += ev.at_ms - t.waitFrom; t.waitFrom = null; }
        const a = asked.get(ev.id);
        if (a) { a.outcome = typeof ev.outcome === "string" ? ev.outcome : "cancelled"; a.record = asks[ev.id] ?? a.record; }
        delete asks[ev.id];
        break;
      }
      case "input.outcome_unknown": note("An answer reached Claude but the result was lost in a restart. The tool may or may not have run.", "warn"); break;
      case "turn.parked":
        for (const id of ev.calls ?? []) { const c = tools.get(id); if (c && !c.done) c.parked = true; }
        break;
      case "subagent.started": if (!ev.parent && !ev.resumed) { helpers.add(ev.id); note(`Started a helper: ${ev.description}`); } break;
      case "subagent.ended": if (helpers.has(ev.id)) note(ev.status === "failed" ? "A helper failed." : "A helper finished."); break;
      case "task.started": note(`Started in the background: ${ev.description}`); break;
      case "task.ended": note(ev.status === "failed" ? "A background task failed." : "A background task finished."); break;
      case "turn.terminal": {
        running = false; thinking = false; reply = null;
        const st = ev.status && typeof ev.status === "object" ? ev.status : { status: ev.status };
        const t = turnOf(ev);
        const edits = new Map();
        for (const c of t.calls) {
          if (c.hidden || (c.done && c.is_error)) continue;
          const e = editOf(c.name, c.input);
          if (!e) continue;
          const f = edits.get(e.path) ?? { add: 0, del: 0 };
          f.add += e.add; f.del += e.del; edits.set(e.path, f);
        }
        let add = 0, del = 0;
        for (const f of edits.values()) { add += f.add; del += f.del; }
        // Calls the turn ended under never finish: they stop being live.
        for (const c of t.calls) if (!c.done) { c.done = true; c.is_error = st.status !== "completed"; }
        push({
          type: "foot", key: "f" + seen, status: st.status ?? "completed", failure: st.failure, detail: st.detail,
          stopped_by: ev.stopped_by, ms: Math.max(0, (ev.at_ms ?? t.start) - t.start - t.waited), files: edits.size, add, del,
        });
        failed = st.status === "failed";
        break;
      }
      case "run.ended": running = false; thinking = false; break;
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
      if (rec.thread === thread && unresolved.has(rec.request?.id)) {
        asks[rec.request.id] = rec;
        const a = asked.get(rec.request.id);
        if (a) a.record = rec;
      }
    }
  }

  $effect(() => untrack(() => log.subscribe((value, change) => {
    const whole = !change || change.replace;
    if (whole) reset();
    const rows = whole ? value.rows : (change.upsert ?? []);
    for (const row of [...rows].sort((x, y) => x.seq - y.seq)) fold(row);
    ask();
  })));

  const status = $derived(unresolved.size ? "needs" : running ? "working" : failed ? "failed" : "idle");
  const title = $derived.by(() => {
    const first = items.find((i) => i.type === "user")?.text ?? "";
    const line = first.split("\n").map((l) => l.trim()).find(Boolean) ?? "";
    return line.length > 90 ? line.slice(0, 88).trimEnd() + "…" : line;
  });
  const place = $derived(folder ? folder.replace(/^\/(Users|home)\/[^/]+/, "~") : "");
  // The live line: what Claude is doing when nothing else on the page shows it.
  const last = $derived(items[items.length - 1]);
  const busyLine = $derived(
    status !== "working" ? ""
    : thinking ? "Thinking"
    : last?.type === "run" && last.calls.some((c) => !c.done && !c.hidden) ? ""
    : last?.type === "agent" ? ""
    : "Working",
  );

  // Follow the end while the reader is there; when they scroll up, stop, and offer the way back.
  let root = $state(null);
  let behind = $state(false);
  function scroller() {
    for (let p = root?.parentElement; p; p = p.parentElement) {
      const o = getComputedStyle(p).overflowY;
      if ((o === "auto" || o === "scroll") && p.scrollHeight > p.clientHeight) return p;
    }
    return document.scrollingElement ?? document.documentElement;
  }
  function atEnd() {
    if (!root) return true;
    const s = scroller();
    return s.scrollHeight - s.scrollTop - s.clientHeight < 96;
  }
  let stuck = true;                // the reader is at the end: growth follows them there
  function follow() {
    stuck = true;
    requestAnimationFrame(() => { const s = scroller(); s.scrollTop = s.scrollHeight; behind = false; });
  }
  $effect(() => {
    if (!root) return;
    const on = () => { stuck = atEnd(); behind = !stuck; };
    addEventListener("scroll", on, { capture: true, passive: true });
    const grew = new ResizeObserver(() => { if (stuck) follow(); else behind = true; });
    grew.observe(root);
    return () => { removeEventListener("scroll", on, { capture: true }); grew.disconnect(); };
  });
</script>

<div class="sc-thread" bind:this={root}>
  <header class="head">
    <div class="titles">
      <h1 class="title">{title || "New thread"}</h1>
      <p class="where">On <a href={`#/h/${encodeURIComponent(host)}`}>{host}</a>{#if place}, in <span class="folder" title={folder}>{place}</span>{/if}</p>
    </div>
    <Pill {status}/>
  </header>

  <div class="log" aria-live="polite">
    {#if log.status === "error" || log.status === "refused" || log.status === "closed"}
      <Empty title="This thread can't be shown right now." body={`${log.message ?? "The host didn't answer."} Check that ${host} is online, then reload.`}/>
    {:else if log.status === "pending" && !items.length}
      <p class="quiet">Loading the conversation…</p>
    {/if}
    {#each items as it (it.key)}
      {#if it.type === "user"}
        <section class="user" aria-label="Your message"><Text text={it.text}/></section>
      {:else if it.type === "agent"}
        <div class="agent"><Text text={it.text}/></div>
      {:else if it.type === "run"}
        <div class="run"><ToolRun calls={it.calls} {folder}/></div>
      {:else if it.type === "ask"}
        {#if !it.outcome && asks[it.id]}
          <div class="ask"><Approval {host} {thread} record={asks[it.id]}/></div>
        {:else if !it.outcome}
          <p class="note" data-tone="needs">Claude is waiting for your answer. Loading the request…</p>
        {:else if it.record}
          <p class="note" data-tone="answer">{OUTCOME[it.outcome] ?? "Withdrawn"}{#if askedFor(it.record)}&nbsp;<code>{askedFor(it.record)}</code>{/if}</p>
        {/if}
      {:else if it.type === "note"}
        <p class="note" data-tone={it.tone}>{it.text}</p>
      {:else if it.type === "foot"}
        <TurnFoot foot={it} {host}/>
      {/if}
    {/each}
    {#if busyLine}<p class="busy"><span class="dot" aria-hidden="true"></span>{busyLine}</p>{/if}
    {#if !items.length && log.status === "live"}
      <Empty title="Nothing here yet." body="Send Claude a message below to start."/>
    {/if}
  </div>

  <div class="dock">
    {#if behind}<button class="jump" type="button" onclick={follow}>Jump to latest</button>{/if}
    <Composer {host} {thread} {status}/>
  </div>
</div>

<style>
  .sc-thread { display: flex; flex-direction: column; min-height: 100%; min-width: 0; }
  .head { display: flex; align-items: flex-start; gap: var(--s3); padding: var(--s4) var(--s5) var(--s3); border-bottom: 1px solid var(--line); }
  .head, .log, .dock > :global(*) { box-sizing: border-box; }
  .titles { flex: 1; min-width: 0; }
  .title { display: -webkit-box; margin: 0; overflow: hidden; font-size: 17px; font-weight: 600; line-height: 1.35; overflow-wrap: anywhere; -webkit-line-clamp: 2; -webkit-box-orient: vertical; }
  .where { margin: 2px 0 0; color: var(--muted); font-size: 13px; overflow-wrap: anywhere; }
  .where a { color: inherit; text-decoration: underline; text-decoration-color: var(--line); text-underline-offset: 3px; }
  .where a:hover { color: var(--ink); }
  .folder { font-family: var(--font-mono); font-size: 12.5px; color: var(--ink); }
  .head :global(.pill) { margin-top: 3px; flex: none; }

  .log { flex: 1; width: 100%; max-width: calc(72ch + 2 * var(--s5)); padding: var(--s4) var(--s5) var(--s6); }
  .user { margin: var(--s5) 0 var(--s4); padding: var(--s3) var(--s4); background: var(--sel); border-radius: var(--radius-md); }
  .user:first-child { margin-top: var(--s2); }
  .agent { margin: var(--s3) 0; max-width: 72ch; }
  .run { margin: var(--s1) 0; }
  .ask { margin: var(--s3) 0; }
  .note { margin: var(--s2) 0; padding-left: var(--s3); color: var(--muted); font-size: 12.5px; border-left: 2px solid var(--line); overflow-wrap: anywhere; }
  .note[data-tone=warn] { color: var(--st-failed-fg); border-left-color: var(--st-failed-fg); }
  .note[data-tone=needs] { color: var(--st-needs-fg); border-left-color: var(--st-needs-fg); }
  .note code { font-family: var(--font-mono); font-size: 12px; color: var(--ink); overflow-wrap: anywhere; }
  .note[data-tone=answer] { border-left-color: var(--st-needs-fg); }
  .quiet { color: var(--muted); }
  .busy { display: flex; align-items: center; gap: var(--s2); margin: var(--s3) 0; color: var(--st-working-fg); font-size: 13px; }
  .dot { width: 7px; height: 7px; border-radius: var(--radius-pill); background: var(--st-working-fg); animation: beat 1.2s ease-out infinite alternate; }

  .dock { position: sticky; bottom: 0; z-index: 1; padding: var(--s3) var(--s5) var(--s4); background: var(--bg); border-top: 1px solid var(--line); }
  .dock > :global(:not(.jump)) { max-width: 72ch; }
  .jump { position: absolute; left: min(50%, calc(var(--s5) + 36ch)); bottom: calc(100% + var(--s3)); transform: translateX(-50%); min-height: 36px; padding: 0 var(--s4); color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-pill); cursor: pointer; }
  .jump:hover { background: var(--hover); }
  .jump:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }

  @keyframes beat { from { opacity: 1; } to { opacity: .35; } }
  @media (prefers-reduced-motion: reduce) { .dot { animation: none; } }
  @media (max-width: 719px) {
    .head { padding: var(--s3) var(--s4); }
    .log { padding: var(--s3) var(--s4) var(--s5); }
    .dock { padding: var(--s2) var(--s4) calc(var(--s3) + env(safe-area-inset-bottom)); }
    .user { padding: var(--s3); }
    .jump { min-height: 44px; }
  }
</style>
