<!-- SUPERCONDUCTOR-11: the inline approval card (SUPERCONDUCTOR-27's 5), where the
     agent stopped: the tool, its input (expandable), and Allow once / Deny. No
     "always": the backend has no rule to keep. A question gets its options as
     buttons; any other request can only be refused from here. The card goes when
     the thread says `input.resolved`. -->
<script module>
  export const uses = ["hosts.answer"];
</script>

<script>
  import { bus } from "plexus-ui/host";

  let { host, thread, record } = $props();
  const b = bus();
  let busy = $state(false);
  let error = $state("");
  let open = $state(false);
  const kind = $derived(record.request?.kind ?? {});
  const shown = $derived(
    kind.input && (kind.input.command ?? kind.input.file_path ?? kind.input.pattern ?? kind.input.url),
  );
  async function answer(a) {
    busy = true;
    error = "";
    const r = await b.call("hosts.answer", { host, thread, request: record.request.id, answer: a });
    const failed = (r.events ?? []).find((e) => e.type === "failed");
    if (!r.ok || failed) { error = failed ? failed.message : (r.message ?? "not answered"); busy = false; }
  }
</script>

<div class="card" data-surface="sc-approval">
  {#if kind.kind === "tool_approval"}
    <div class="what">Claude wants to use <b>{kind.tool}</b>{#if shown}: <code>{shown}</code>{/if}</div>
    {#if kind.description}<div class="muted">{kind.description}</div>{/if}
    <button class="link" type="button" onclick={() => (open = !open)}>{open ? "hide input" : "show input"}</button>
    {#if open}<pre>{JSON.stringify(kind.input, null, 2)}</pre>{/if}
    {#if record.after_unknown_outcome}<div class="error">An earlier answer's outcome is unknown: it may have run already.</div>{/if}
    <div class="actions">
      <button class="primary" type="button" disabled={busy} onclick={() => answer({ answer: "allow" })}>Allow once</button>
      <button type="button" disabled={busy} onclick={() => answer({ answer: "deny", message: "Denied from the browser.", stop_turn: false })}>Deny</button>
    </div>
  {:else if kind.kind === "question"}
    {#each kind.questions ?? [] as q (q.question)}
      <div class="what">{q.question}</div>
      <div class="actions">
        {#each q.options ?? [] as o (o.label)}
          <button type="button" title={o.description ?? ""} disabled={busy} onclick={() => answer({ answer: "answers", answers: { [q.question]: o.label } })}>{o.label}</button>
        {/each}
      </div>
    {/each}
  {:else}
    <div class="what">Claude asks for “{kind.name ?? kind.kind}”, which this page can't answer.</div>
    <div class="actions"><button type="button" disabled={busy} onclick={() => answer({ answer: "refuse", reason: "the browser can't answer this" })}>Refuse</button></div>
  {/if}
  {#if error}<div class="error">{error}</div>{/if}
</div>

<style>
  .card { display: grid; gap: 8px; margin: 8px 0; padding: 12px 14px; background: var(--st-needs-bg); color: var(--ink); border: var(--border) solid var(--st-needs-fg); border-radius: var(--radius-lg); }
  .what { overflow-wrap: anywhere; }
  code, pre { font-family: var(--font-text); font-size: 12px; }
  pre { margin: 0; padding: 8px; max-height: 240px; overflow: auto; white-space: pre-wrap; background: var(--card); border-radius: var(--radius); }
  .muted { color: var(--muted); }
  .error { color: var(--warn); }
  .actions { display: flex; gap: 8px; flex-wrap: wrap; }
  button { padding: 5px 14px; background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius); cursor: pointer; }
  .primary { color: var(--on-accent); background: var(--accent); border-color: var(--accent); }
  button:disabled { opacity: .6; cursor: default; }
  .link { justify-self: start; padding: 0; color: var(--accent); background: none; border: none; font-size: 12px; }
  @media (max-width: 700px) { .actions { flex-direction: column; } .actions button { width: 100%; } }
</style>
