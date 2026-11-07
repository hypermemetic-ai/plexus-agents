<!-- SC-31: a run of consecutive tool calls as ONE sentence (SUPERCONDUCTOR-27):
     "Ran 2 commands, read 3 files, edited 1 file  +12 −3", a tap away from each
     call (sc-tool). A run of one call is that call alone. While a call runs, the
     sentence is live and the running call shows under it, open or not. -->
<script module>
  export const uses = [];
  import { kindOf } from "./sc-tool.svelte";
  import { editOf } from "./sc-diff.svelte";

  const n = (k, one, many) => `${k} ${k === 1 ? one : many}`;

  /** The run in words, and its +/− totals. */
  export function summarize(calls) {
    const c = { command: 0, read: new Set(), search: 0, edit: new Set(), todo: 0, fetch: 0, websearch: 0, helper: 0, other: 0 };
    let add = 0, del = 0, failed = 0;
    for (const call of calls) {
      const k = kindOf(call.name);
      if (call.done && call.is_error) failed++;
      if (k === "read") c.read.add(call.input?.file_path ?? call.id);
      else if (k === "edit") {
        const e = editOf(call.name, call.input);
        c.edit.add(e?.path || call.id);
        if (e && !(call.done && call.is_error)) { add += e.add; del += e.del; }
      } else c[k]++;
    }
    const parts = [];
    if (c.command) parts.push(`ran ${n(c.command, "command", "commands")}`);
    if (c.read.size) parts.push(`read ${n(c.read.size, "file", "files")}`);
    if (c.search) parts.push(`searched ${c.search === 1 ? "once" : c.search + " times"}`);
    if (c.edit.size) parts.push(`edited ${n(c.edit.size, "file", "files")}`);
    if (c.todo) parts.push("updated the plan");
    if (c.fetch) parts.push(`fetched ${n(c.fetch, "page", "pages")}`);
    if (c.websearch) parts.push(`searched the web${c.websearch > 1 ? " " + c.websearch + " times" : ""}`);
    if (c.helper) parts.push(`asked ${n(c.helper, "helper", "helpers")}`);
    if (c.other) parts.push(`used ${n(c.other, "other tool", "other tools")}`);
    let text = parts.join(", ");
    text = text.charAt(0).toUpperCase() + text.slice(1);
    return { text, add, del, failed, edits: c.edit.size };
  }
</script>

<script>
  import Tool from "./sc-tool.svelte";

  let { calls = [], folder = "" } = $props();
  let open = $state(false);
  const shown = $derived(calls.filter((c) => !c.hidden));
  const sum = $derived(summarize(shown));
  const live = $derived(shown.filter((c) => !c.done));
</script>

{#if shown.length === 1}
  <Tool call={shown[0]} {folder}/>
{:else if shown.length}
  <div class="sc-toolrun" data-live={live.length > 0}>
    <button class="summary" type="button" aria-expanded={open} onclick={() => (open = !open)}>
      <span class="chev" aria-hidden="true">{open ? "▾" : "▸"}</span>
      <span class="said">{sum.text}{#if sum.failed}<span class="bad">, {sum.failed} failed</span>{/if}</span>
      {#if sum.add || sum.del}<span class="counts"><span class="add">+{sum.add}</span> <span class="del">−{sum.del}</span></span>{/if}
    </button>
    {#if open}
      <div class="calls">{#each shown as c (c.id)}<Tool call={c} {folder}/>{/each}</div>
    {:else if live.length}
      <div class="calls">{#each live as c (c.id)}<Tool call={c} {folder}/>{/each}</div>
    {/if}
  </div>
{/if}

<style>
  .sc-toolrun { min-width: 0; }
  .summary { display: flex; align-items: baseline; gap: var(--s2); width: 100%; min-height: 32px; padding: var(--s1) var(--s2); text-align: left; color: var(--muted); background: none; border: 0; border-radius: var(--radius-sm); cursor: pointer; }
  .summary:hover { color: var(--ink); background: var(--hover); }
  .summary:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }
  .chev { flex: none; width: 1em; }
  .said { min-width: 0; }
  .bad { color: var(--st-failed-fg); }
  .counts { flex: none; margin-left: auto; font-family: var(--font-mono); font-size: 12px; }
  .add { color: var(--diff-add-fg); }
  .del { color: var(--diff-del-fg); }
  .calls { margin-left: var(--s3); padding-left: var(--s2); border-left: 1px solid var(--line); }
  @media (max-width: 719px) { .summary { min-height: 44px; } .calls { margin-left: var(--s1); } }
</style>
