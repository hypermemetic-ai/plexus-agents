<!-- SC-32: the approval card, inline where the agent stopped (SUPERCONDUCTOR-27's 4).
     The one place the amber "needs you" signal is strongest: an amber edge and
     head. It says in words what Claude wants to do — a command as a command, an
     edit as a diff, a write as its file — and offers what the backend can do
     with it (plexus-harness InputAnswer): Allow once, or Deny (Claude is told,
     and goes on). superconductor-ui/modes: when the request carries Claude's
     own rule suggestions (`request.raw.request.permission_suggestions`), each
     one it can keep becomes an "Always allow …" button beside Allow once
     (hosts.answer with `apply: [i]`), with a line saying in words what is
     remembered and for how long: this thread, from now on; other threads still
     ask. A switch to bypass is never offered here (that is the thread header's,
     behind its confirm). `rules` false (a host from before modes) hides them.
     A note with a denial goes from the
     composer: typing a reply while this is pending offers "Deny and send
     this" there.

     Keys while it waits (bus.keys, the newest card alone): `y` allows once,
     `a` always allows (the first suggestion), `n` denies; a question's options are `1`…`9`. The chosen button fills and
     the others fade; once answered the card collapses to one line, which it
     keeps if the transcript keeps the record (`record.outcome`, the
     `input.resolved` outcome).

     A card the composer answered (Deny and send this) collapses by the
     record's outcome like any other. -->
<script module>
  export const uses = ["hosts.answer"];

  const base = (p) => String(p ?? "").split("/").filter(Boolean).pop() ?? String(p ?? "");
  const str = (v) => (typeof v === "string" ? v : v == null ? "" : JSON.stringify(v, null, 2));

  /** What a tool approval asks, in words: a head, the thing in mono, and how to show its input. */
  export function describe(kind) {
    const i = kind.input ?? {};
    switch (kind.tool) {
      case "Bash":
        return { head: "Run a command", show: "command", code: str(i.command), lang: "sh", note: i.description, line: str(i.command) };
      case "Edit":
        return { head: "Edit", path: i.file_path, show: "diff", edits: [{ old: str(i.old_string), new: str(i.new_string) }], line: base(i.file_path) };
      case "MultiEdit":
        return { head: "Edit", path: i.file_path, show: "diff", edits: (i.edits ?? []).map((e) => ({ old: str(e.old_string), new: str(e.new_string) })), line: base(i.file_path) };
      case "Write":
        return { head: "Write", path: i.file_path, show: "code", code: str(i.content), lang: (String(i.file_path ?? "").split(".").pop() ?? ""), line: base(i.file_path) };
      case "NotebookEdit":
        return { head: "Edit the notebook", path: i.notebook_path, show: "code", code: str(i.new_source), line: base(i.notebook_path) };
      case "Read":
        return { head: "Read", path: i.file_path, show: "none", line: base(i.file_path) };
      case "WebFetch":
        return { head: "Fetch a web page", show: "command", code: str(i.url), note: i.prompt, line: str(i.url) };
      case "WebSearch":
        return { head: "Search the web", show: "command", code: str(i.query), line: str(i.query) };
      case "ExitPlanMode":
        return { head: "Start on this plan", show: "text", text: str(i.plan), line: "the plan" };
      default:
        return { head: `Use ${kind.tool ?? "a tool"}`, show: "code", code: JSON.stringify(i, null, 2), lang: "json", line: kind.tool ?? "a tool" };
    }
  }
  const SAID = { allowed: "Allowed once", always: "Allowed, and remembered for this thread", denied: "Denied", answered: "Answered", refused: "Refused" };

  /** The request's own rule suggestions, whole, in its order (the index `apply` takes). */
  export function suggestionsOf(record) {
    const raw = record?.request?.raw ?? {};
    const body = raw.request && typeof raw.request === "object" ? raw.request : raw;
    return Array.isArray(body.permission_suggestions) ? body.permission_suggestions : [];
  }
  const MODE_WORDS = { default: "ask before each action", acceptEdits: "auto-accept edits", plan: "plan only" };
  const EDITS = ["Edit", "Write", "MultiEdit", "NotebookEdit"];
  /** One rule (`{toolName, ruleContent}`) in words: what Claude won't ask about again. */
  function ruleWords(r) {
    const tool = r?.toolName ?? "", c = r?.ruleContent ?? "";
    if (tool === "Bash") {
      if (!c) return { button: "Always allow commands", text: "running any command" };
      const prefix = c.match(/^(.*?)(:\*| \*)$/);
      if (prefix) return { button: "Always allow commands like this", text: "running commands that start with", code: prefix[1] };
      return { button: "Always allow this command", text: "running", code: c };
    }
    if (EDITS.includes(tool)) return c ? { button: "Always allow these edits", text: "editing", code: c } : { button: "Always allow edits", text: "editing files" };
    if (tool === "Read") return c ? { button: "Always allow reading these", text: "reading", code: c } : { button: "Always allow reading", text: "reading files" };
    if (tool === "WebFetch") return c ? { button: "Always allow this site", text: "fetching from", code: c.replace(/^domain:/, "") } : { button: "Always allow fetching", text: "fetching web pages" };
    return { button: `Always allow ${tool || "this"}`, text: `using ${tool || "this tool"}`, code: c || undefined };
  }
  /** What one suggestion offers: a button and what it remembers, or null when it isn't one to offer here. */
  export function offerOf(sg) {
    switch (sg?.type) {
      case "addRules": {
        if (sg.behavior && sg.behavior !== "allow") return null;
        const rules = sg.rules ?? [];
        if (!rules.length) return null;
        const w = ruleWords(rules[0]);
        if (rules.length > 1) return { button: "Always allow these", text: rules.map((r) => { const x = ruleWords(r); return x.code ? `${x.text} ${x.code}` : x.text; }).join("; ") };
        return w;
      }
      case "addDirectories": {
        const dirs = sg.directories ?? [];
        if (!dirs.length) return null;
        return { button: "Always allow this folder", text: "working in", code: dirs.join(", ") };
      }
      case "setMode":
        if (!MODE_WORDS[sg.mode]) return null;  // bypass, or a mode this page doesn't know
        return { button: sg.mode === "acceptEdits" ? "Allow, and auto-accept edits" : `Allow, and ${MODE_WORDS[sg.mode]}`, text: `switching this thread to ${MODE_WORDS[sg.mode]}`, mode: true };
      default:
        return null;
    }
  }
  /** The one-line record of an outcome (`input.resolved`'s, or this card's own). */
  export function said(outcome) {
    if (!outcome) return "";
    if (typeof outcome === "object" && outcome.cancelled) return outcome.cancelled === "interrupted" ? "Cancelled: the turn was stopped" : "Cancelled: the request went away";
    return SAID[outcome] ?? String(outcome);
  }
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Button from "./sc-button.svelte";
  import CodeCard from "./code-card.svelte";
  import Diff from "./sc-diff.svelte";
  import Text from "./sc-text.svelte";

  let { host, thread, record, rules = true } = $props();
  const b = bus();
  const kind = $derived(record.request?.kind ?? {});
  const id = $derived(record.request?.id);
  const what = $derived(kind.kind === "tool_approval" ? describe(kind) : null);
  // Claude's suggestions it can keep, each with its index into the request's list.
  const offers = $derived(
    rules && kind.kind === "tool_approval"
      ? suggestionsOf(record).map((sg, i) => ({ i, ...(offerOf(sg) ?? {}) })).filter((o) => o.button).slice(0, 3)
      : [],
  );

  let busy = $state("");           // the answer under way: "allow" | "deny" | an option's label
  let mine = $state(null);         // { outcome, note } once answered from this page
  let error = $state("");
  const outcome = $derived(mine?.outcome ?? record.outcome ?? null);
  const done = $derived(!!outcome);

  async function answer(a, chosen, as, apply) {
    if (busy || done) return;
    busy = chosen;
    error = "";
    const params = { host, thread, request: id, answer: a };
    if (apply) params.apply = apply;
    const r = await b.call("hosts.answer", params);
    const failed = (r.events ?? []).find((e) => e.type === "failed");
    busy = "";
    if (!r.ok || failed) {
      const why = failed?.message ?? r.message ?? "no reply";
      error = r.message === "disconnected"
        ? "Your answer didn't reach the server: the connection dropped. Check you're online, then answer again."
        : `Your answer wasn't taken: ${why}. If it was answered somewhere else, this card goes when that lands; otherwise answer again.`;
      return;
    }
    mine = { outcome: as };
  }
  const allow = () => answer({ answer: "allow" }, "allow", "allowed");
  const always = (o) => answer({ answer: "allow" }, "always" + o.i, "always", [o.i]);
  const deny = () => answer({ answer: "deny", message: "The user denied this from the browser.", stop_turn: false }, "deny", "denied");
  const pick = (q, o) => answer({ answer: "answers", answers: { [q.question]: o.label } }, o.label, "answered");
  const refuse = () => answer({ answer: "refuse", reason: "the browser can't answer this kind of request" }, "refuse", "refused");

  // y / n (or 1…9 for a question) while it waits; the newest card alone.
  $effect(() => {
    if (done) return;
    const map = {};
    if (kind.kind === "tool_approval") {
      map.y = { to: allow, label: "allow once", only: true };
      if (offers.length) map.a = { to: () => always(offers[0]), label: offers[0].button.toLowerCase(), only: true };
      map.n = { to: deny, label: "deny", only: true };
    } else if (kind.kind === "question") {
      const opts = (kind.questions?.[0]?.options ?? []).slice(0, 9);
      opts.forEach((o, n) => { map[String(n + 1)] = { to: () => pick(kind.questions[0], o), label: `answer “${o.label}”`, only: true }; });
    }
    return b.keys(map);
  });
</script>

{#if done}
  <div class="record" data-surface="sc-approval" data-outcome={typeof outcome === "string" ? outcome : "cancelled"}>
    <span class="mark" aria-hidden="true">{outcome === "allowed" || outcome === "answered" || outcome === "always" ? "✓" : "✕"}</span>
    <span class="said">{said(outcome)}</span>
    {#if what}<span class="obj">{what.line}</span>{/if}
  </div>
{:else}
  <section class="card" data-surface="sc-approval" aria-label="Claude needs your answer">
    {#if kind.kind === "tool_approval"}
      <header>
        <span class="ask">Claude wants to</span>
        <h3>{what.head}{#if what.path} <code class="path" title={what.path}>{what.path}</code>{/if}</h3>
      </header>
      {#if kind.description && kind.description !== what.note}<p class="why">{kind.description}</p>{/if}
      {#if what.note}<p class="why">{what.note}</p>{/if}
      {#if what.show === "command"}
        <CodeCard code={what.code} lang={what.lang ?? ""} wrap={true}/>
      {:else if what.show === "diff"}
        {#each what.edits as e, n (n)}<Diff old={e.old} new={e.new} path={what.path}/>{/each}
      {:else if what.show === "code"}
        <div class="scroll"><CodeCard code={what.code} lang={what.lang ?? ""} wrap={true}/></div>
      {:else if what.show === "text"}
        <div class="scroll plan"><Text text={what.text}/></div>
      {/if}
      {#if record.after_unknown_outcome}
        <p class="warn">An earlier answer to this may already have run it. Check before you allow it again.</p>
      {/if}
      <div class="actions" data-busy={busy || null}>
        <span class="act" data-chosen={busy === "allow" || null}><Button kind="primary" disabled={!!busy} onclick={allow}>{busy === "allow" ? "Allowing…" : "Allow once"}<kbd>y</kbd></Button></span>
        {#each offers as o, n (o.i)}
          <span class="act always" data-chosen={busy === "always" + o.i || null}><Button kind="quiet" disabled={!!busy} onclick={() => always(o)}>{busy === "always" + o.i ? "Allowing…" : o.button}{#if n === 0}<kbd>a</kbd>{/if}</Button></span>
        {/each}
        <span class="act" data-chosen={busy === "deny" || null}><Button kind="quiet" disabled={!!busy} onclick={deny}>{busy === "deny" ? "Denying…" : "Deny"}<kbd>n</kbd></Button></span>
      </div>
      {#if offers.length}
        <div class="keeps">
          {#each offers as o (o.i)}
            <p><strong>{o.button}</strong> also allows {o.text}{#if o.code}{" "}<code>{o.code}</code>{/if} without asking, in this thread from now on{o.mode ? "" : ", restarts included"}. Other threads still ask.</p>
          {/each}
        </div>
      {/if}
      <p class="hint">To deny with a reason, type it in the reply box below.</p>
    {:else if kind.kind === "question"}
      <header><span class="ask">Claude asks</span></header>
      {#each kind.questions ?? [] as q, qi (q.question)}
        {#if q.header}<span class="ask">{q.header}</span>{/if}
        <h3 class="q">{q.question}</h3>
        <div class="options">
          {#each q.options ?? [] as o, n (o.label)}
            <span class="act" data-chosen={busy === o.label || null}>
              <Button kind="quiet" disabled={!!busy} onclick={() => pick(q, o)}>
                <span class="opt"><span>{o.label}</span>{#if o.description}<small>{o.description}</small>{/if}</span>
                {#if qi === 0 && n < 9}<kbd>{n + 1}</kbd>{/if}
              </Button>
            </span>
          {/each}
        </div>
      {/each}
      <p class="hint">To answer in your own words, type it in the reply box below.</p>
    {:else}
      <header>
        <span class="ask">Claude asks for</span>
        <h3>{kind.name ?? kind.kind}</h3>
      </header>
      <p class="why">This page can't answer this kind of request. Refuse it to let Claude go on, or answer it from the terminal.</p>
      <div class="actions"><span class="act"><Button kind="quiet" disabled={!!busy} onclick={refuse}>Refuse</Button></span></div>
    {/if}
    {#if error}<p class="warn" role="alert">{error}</p>{/if}
  </section>
{/if}

<style>
  .card {
    display: grid; gap: var(--s3); max-width: 72ch; margin: var(--s4) 0; padding: var(--s4) var(--s4) var(--s4) calc(var(--s4) + 2px);
    color: var(--ink); background: var(--panel);
    border: 1px solid var(--st-needs-fg); border-left: 6px solid var(--st-needs-fg); border-radius: var(--radius-md);
    min-width: 0;
  }
  header { display: grid; gap: 2px; min-width: 0; }
  .ask { font-size: 13px; color: var(--st-needs-fg); font-weight: 500; }
  h3 { margin: 0; font-size: 17px; font-weight: 600; line-height: 1.3; overflow-wrap: anywhere; }
  .q { font-size: 16px; }
  .path { font: 500 15px/1.3 var(--font-mono); padding: 0 var(--s1); background: var(--st-needs-bg); border-radius: var(--radius-xs); overflow-wrap: anywhere; }
  .why { margin: 0; color: var(--muted); line-height: var(--leading); }
  .scroll { max-height: 320px; overflow: auto; overscroll-behavior: contain; border-radius: var(--radius-sm); }
  .card :global(button) { -webkit-tap-highlight-color: transparent; touch-action: manipulation; }
  .plan { padding: var(--s3); background: var(--card); border: 1px solid var(--line); }
  .warn { margin: 0; color: var(--st-failed-fg); background: var(--st-failed-bg); padding: var(--s2) var(--s3); border-radius: var(--radius-sm); }
  .hint { margin: 0; font-size: 13px; color: var(--muted); }
  .actions, .options { display: flex; flex-wrap: wrap; gap: var(--s2); }
  .options { display: grid; grid-template-columns: repeat(auto-fill, minmax(180px, 1fr)); }
  .act { display: contents; }
  .act :global(button) { min-height: 44px; display: inline-flex; align-items: center; justify-content: center; gap: var(--s2); }
  .options .act :global(button) { justify-content: space-between; text-align: left; padding-top: var(--s2); padding-bottom: var(--s2); }
  .opt { display: grid; }
  .opt small { color: var(--muted); font-size: 12px; font-weight: 400; }
  [data-busy] .act:not([data-chosen]) :global(button) { opacity: .45; }
  .options:has([data-chosen]) .act:not([data-chosen]) :global(button) { opacity: .45; }
  [data-chosen] :global(button) { color: var(--on-accent); background: var(--accent); border-color: var(--accent); }
  kbd { font: 500 11px/16px var(--font-mono); min-width: 18px; padding: 0 var(--s1); text-align: center; border: 1px solid currentColor; border-radius: var(--radius-xs); opacity: .65; }

  .record {
    display: flex; align-items: baseline; gap: var(--s2); max-width: 72ch; margin: var(--s2) 0; padding: var(--s1) var(--s3);
    font-size: 13px; color: var(--muted); border-left: 2px solid var(--line); min-width: 0;
  }
  .record .said { color: var(--ink); font-weight: 500; white-space: nowrap; }
  .record[data-outcome=allowed] .mark, .record[data-outcome=answered] .mark, .record[data-outcome=always] .mark { color: var(--st-done-fg); }
  .keeps { display: grid; gap: var(--s1); padding: var(--s2) var(--s3); font-size: var(--text-note); line-height: var(--leading); color: var(--muted); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); }
  .keeps p { margin: 0; overflow-wrap: anywhere; }
  .keeps strong { color: var(--ink); font-weight: var(--weight-medium); }
  .keeps code { font: 12px var(--font-mono); color: var(--ink); }
  .record[data-outcome=denied] .mark, .record[data-outcome=refused] .mark { color: var(--st-failed-fg); }
  .obj { font-family: var(--font-mono); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }

  @media (hover: none), (max-width: 719px) {
    kbd { display: none; }
  }
  @media (max-width: 719px) {
    .card { margin-left: calc(-1 * var(--s2)); margin-right: calc(-1 * var(--s2)); padding: var(--s3) var(--s3) var(--s3) calc(var(--s3) + 2px); }
    .actions { display: grid; grid-template-columns: 1fr 1fr; }
    .actions .always { display: block; grid-column: 1 / -1; grid-row: 2; }
    .actions .act :global(button) { min-height: 48px; width: 100%; }
    .options { grid-template-columns: 1fr; }
  }
</style>
