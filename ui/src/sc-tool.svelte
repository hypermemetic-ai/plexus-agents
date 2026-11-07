<!-- SC-31: one tool call, drawn for what it is (SUPERCONDUCTOR-27, CloudCLI's
     per-tool rendering): a file read or a search is a one-line row with its path
     or pattern; a command is a command row in mono with its exit status and a
     chevron to its output (code-card, capped, "Show all"); an edit or a write is
     an sc-diff card; TodoWrite is a checklist; anything else is its name, its
     target and its input as JSON. A running call says so live. No tool_use id
     is ever shown. Text only. -->
<script module>
  export const uses = [];

  /** What a call is, for the row and for sc-toolrun's sentence. */
  export function kindOf(name) {
    switch (name) {
      case "Bash": case "BashOutput": return "command";
      case "Read": case "NotebookRead": return "read";
      case "Grep": case "Glob": case "LS": return "search";
      case "Edit": case "MultiEdit": case "Write": case "NotebookEdit": return "edit";
      case "TodoWrite": return "todo";
      case "WebFetch": return "fetch";
      case "WebSearch": return "websearch";
      case "Task": case "Agent": return "helper";
      default: return "other";
    }
  }

  /** A Bash result's exit status, read from Claude's words for it. */
  export function exitOf(call) {
    if (!call.done) return null;
    const m = /^Exit code (\d+)/.exec(textOf(call.output));
    return m ? Number(m[1]) : call.is_error ? 1 : 0;
  }

  export function textOf(output) {
    if (output == null) return "";
    if (typeof output === "string") return output;
    if (Array.isArray(output)) return output.map((p) => (typeof p === "string" ? p : p?.text ?? "")).join("\n");
    return JSON.stringify(output, null, 2);
  }
</script>

<script>
  import CodeCard from "./code-card.svelte";
  import Diff, { relPath } from "./sc-diff.svelte";

  let { call, folder = "" } = $props();
  const CAP = 3000;
  const LINES = 40;
  const kind = $derived(kindOf(call.name));
  const input = $derived(call.input ?? {});
  const failed = $derived(call.done && call.is_error);
  const running = $derived(!call.done);
  const out = $derived(textOf(call.output).replace(/^Exit code \d+\n?/, ""));
  const exit = $derived(kind === "command" ? exitOf(call) : null);
  let open = $state(false);
  let all = $state(false);
  const capped = $derived.by(() => {
    if (all) return out;
    const lines = out.split("\n");
    if (lines.length > LINES) return lines.slice(0, LINES).join("\n");
    return out.length > CAP ? out.slice(0, CAP) : out;
  });
  const more = $derived(capped.length < out.length);
  const rel = (p) => relPath(p, folder);
  const target = $derived(
    input.file_path ?? input.notebook_path ?? input.path ?? input.pattern ?? input.url ?? input.query ?? input.command ?? input.description ?? "",
  );
  const todos = $derived(Array.isArray(input.todos) ? input.todos : []);
  const lang = $derived(kind === "read" ? String(input.file_path ?? "").split(".").pop() : "");
  const outLang = $derived(kind === "command" ? "output" : kind === "read" ? lang : "");
</script>

{#if kind === "edit" && call.name !== "NotebookEdit"}
  <Diff name={call.name} input={input} {folder} {failed} {running}/>
  {#if failed && out}<p class="why">{out}</p>{/if}
{:else if kind === "todo"}
  <div class="tool todo" data-state={running ? "running" : failed ? "failed" : "done"}>
    <p class="head">Updated the plan</p>
    <ul class="checklist">
      {#each todos as t, k (k)}
        <li data-status={t.status}>
          <span class="box" aria-hidden="true">{t.status === "completed" ? "✓" : ""}</span>
          <span class="item">{t.status === "in_progress" ? t.activeForm ?? t.content : t.content}</span>
          <span class="sr">{t.status === "completed" ? "done" : t.status === "in_progress" ? "in progress" : "to do"}</span>
        </li>
      {/each}
    </ul>
  </div>
{:else}
  <div class="tool" data-kind={kind} data-state={running ? "running" : failed ? "failed" : "done"}>
    <button class="row" type="button" aria-expanded={open} onclick={() => (open = !open)}>
      <span class="state" aria-hidden="true"></span>
      {#if kind === "command"}
        <span class="cmd">{input.command ?? ""}</span>
        <span class="tail">{running ? "running" : exit === 0 ? "" : `exit ${exit}`}</span>
      {:else if kind === "read"}
        <span class="verb">{running ? "Reading" : failed ? "Could not read" : "Read"}</span><span class="arg">{rel(target)}</span>
        {#if input.offset || input.limit}<span class="tail">{input.offset ? `from line ${input.offset}` : `first ${input.limit} lines`}</span>{/if}
      {:else if kind === "search"}
        <span class="verb">{call.name === "Glob" ? (running ? "Finding files" : "Found files") : call.name === "LS" ? "Listed" : running ? "Searching for" : "Searched for"}</span><span class="arg">{call.name === "LS" ? rel(target) : input.pattern ?? ""}</span>
        {#if input.path && call.name !== "LS"}<span class="tail">in {rel(input.path) || "the folder"}</span>{/if}
      {:else if kind === "fetch"}
        <span class="verb">{running ? "Fetching" : "Fetched"}</span><span class="arg">{input.url ?? ""}</span>
      {:else if kind === "websearch"}
        <span class="verb">{running ? "Searching the web for" : "Searched the web for"}</span><span class="arg prose">{input.query ?? ""}</span>
      {:else if kind === "helper"}
        <span class="verb">{running ? "A helper is working on" : "A helper worked on"}</span><span class="arg prose">{input.description ?? ""}</span>
      {:else}
        <span class="verb">{call.name}</span>{#if target}<span class="arg">{typeof target === "string" ? rel(target) : ""}</span>{/if}
      {/if}
      <span class="chev" aria-hidden="true">{open ? "▾" : "▸"}</span>
    </button>
    {#if open}
      <div class="detail">
        {#if kind === "other" || kind === "helper"}
          <CodeCard code={JSON.stringify(input, null, 2)} lang="json" wrap/>
        {/if}
        {#if kind === "command" && input.description}<p class="note">{input.description}</p>{/if}
        {#if running}
          <p class="note">Still running.</p>
        {:else if out}
          <CodeCard code={capped} lang={outLang} wrap={kind !== "read"}/>
          {#if more}<button class="more" type="button" onclick={() => (all = true)}>Show all {out.split("\n").length} lines</button>{/if}
        {:else}
          <p class="note">No output.</p>
        {/if}
      </div>
    {/if}
  </div>
{/if}

<style>
  .tool { min-width: 0; }
  .row { display: flex; align-items: baseline; gap: var(--s2); width: 100%; min-height: 32px; padding: var(--s1) var(--s2); text-align: left; color: var(--ink); background: none; border: 0; border-radius: var(--radius-sm); cursor: pointer; }
  @media (hover: hover) { .row:hover { background: var(--hover); } }
  .row, .more { -webkit-tap-highlight-color: transparent; }
  .row:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }
  .state { flex: none; align-self: center; width: 7px; height: 7px; border-radius: var(--radius-pill); background: var(--line); }
  .tool[data-state=running] .state { background: var(--st-working-fg); animation: beat 1.2s ease-out infinite alternate; }
  .tool[data-state=failed] .state { background: var(--st-failed-fg); }
  .verb { flex: none; color: var(--muted); }
  .arg { min-width: 0; overflow-wrap: anywhere; font-family: var(--font-mono); font-size: 12.5px; }
  .arg.prose { font-family: var(--font-text); font-size: inherit; }
  .cmd { min-width: 0; overflow-wrap: anywhere; white-space: pre-wrap; font-family: var(--font-mono); font-size: 12.5px; }
  .cmd::before { content: "$ "; color: var(--muted); }
  .tail { flex: none; color: var(--muted); font-size: 12px; }
  .tool[data-state=failed] .tail { color: var(--st-failed-fg); }
  .chev { flex: none; margin-left: auto; padding-left: var(--s2); color: var(--muted); }
  .detail { padding: 0 0 var(--s2) var(--s5); min-width: 0; }
  .note { margin: var(--s1) 0; color: var(--muted); font-size: 12.5px; }
  .more { min-height: 32px; padding: 0; color: var(--accent); background: none; border: 0; cursor: pointer; }
  .why { margin: 0 0 var(--s2) var(--s3); color: var(--st-failed-fg); font-size: 12.5px; overflow-wrap: anywhere; }
  .todo { margin: var(--s1) 0; padding: var(--s2) var(--s3); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-md); }
  .head { margin: 0 0 var(--s1); color: var(--muted); }
  .checklist { margin: 0; padding: 0; list-style: none; }
  .checklist li { display: flex; gap: var(--s2); align-items: baseline; padding: 2px 0; }
  .box { flex: none; display: inline-grid; place-items: center; width: 14px; height: 14px; font-size: 10px; line-height: 1; color: var(--on-accent); border: 1px solid var(--muted); border-radius: var(--radius-xs); transform: translateY(2px); }
  li[data-status=completed] .box { background: var(--st-done-fg); border-color: var(--st-done-fg); }
  li[data-status=completed] .item { color: var(--muted); text-decoration: line-through; }
  li[data-status=in_progress] .box { border: 4px solid var(--st-working-fg); }
  li[data-status=in_progress] .item { font-weight: 500; }
  .sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); }
  @keyframes beat { from { opacity: 1; } to { opacity: .35; } }
  @media (prefers-reduced-motion: reduce) { .tool .state { animation: none !important; } }
  @media (max-width: 719px) { .detail { padding-left: 0; } .row { min-height: 44px; } }
</style>
