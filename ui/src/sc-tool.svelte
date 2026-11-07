<!-- SUPERCONDUCTOR-11: one tool call as a one-line row — the tool, its target
     (a command, a file, a pattern, a URL), running / ✔ / ✗ — that expands to its
     input and its output, the output capped with "show all". Text, never HTML. -->
<script module>
  export const uses = [];
</script>

<script>
  let { call } = $props();
  const CAP = 4000;
  let open = $state(false);
  let all = $state(false);
  const target = $derived(
    call.input && (call.input.command ?? call.input.file_path ?? call.input.path ?? call.input.pattern ?? call.input.url ?? call.input.description),
  );
  const output = $derived(call.output == null ? "" : typeof call.output === "string" ? call.output : JSON.stringify(call.output, null, 2));
</script>

<div class="tool" data-done={call.done} data-error={call.is_error}>
  <button class="line" type="button" onclick={() => (open = !open)} aria-expanded={open}>
    <span class="mark">{call.done ? (call.is_error ? "✗" : "✔") : "…"}</span>
    <b>{call.name}</b>{#if target}<span class="target">{target}</span>{/if}
  </button>
  {#if open}
    <pre class="input">{JSON.stringify(call.input, null, 2)}</pre>
    {#if call.done}
      <pre class="output">{all || output.length <= CAP ? (output || "(no output)") : output.slice(0, CAP) + "…"}</pre>
      {#if output.length > CAP && !all}<button class="more" type="button" onclick={() => (all = true)}>show all {output.length} characters</button>{/if}
    {:else}
      <p class="muted">running…</p>
    {/if}
  {/if}
</div>

<style>
  .tool { margin: 1px 0; }
  .line { display: flex; gap: 8px; align-items: baseline; width: 100%; padding: 2px 6px; text-align: left; color: var(--ink); font: 12px/1.6 var(--font-text); background: none; border: none; border-radius: var(--radius); cursor: pointer; }
  .line:hover { background: var(--hover); }
  .mark { width: 1em; color: var(--muted); }
  .tool[data-done=true] .mark { color: var(--live); }
  .tool[data-error=true] .mark { color: var(--warn); }
  .target { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); }
  pre { margin: 2px 0 6px 22px; padding: 8px; max-height: 360px; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; font: 12px/1.5 var(--font-text); background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius); }
  .tool[data-error=true] .output { color: var(--warn); }
  .more { margin-left: 22px; padding: 0; color: var(--accent); background: none; border: none; font-size: 12px; cursor: pointer; }
  .muted { margin: 2px 0 6px 22px; color: var(--muted); font-size: 12px; }
</style>
