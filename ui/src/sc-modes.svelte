<!-- superconductor-ui/modes: the permission mode picker, shared by the new thread
     page and the thread header (sc-newthread, sc-thread). Four choices in plain
     words, each with one line on what it means; "Bypass all permissions" is the
     dangerous one: brick, behind an inline confirm that says what it does, and
     disabled with the way to turn it on when the host doesn't allow it
     (`hosts.capabilities` `bypass`, from `plexus-agents host --allow-bypass`).

     Props: `value` (a mode), `onpick(mode)` (called once a mode is chosen, after
     the confirm for bypass), `bypass` (true, false, or null when the host doesn't
     say), `host` (its name, for the words), `busy` (picking is under way),
     `label` (the group's accessible name). -->
<script module>
  export const uses = [];

  /** The modes in the order offered, as the operator would name them. */
  export const MODES = [
    { mode: "default", label: "Ask before each action", note: "Claude asks you before it edits a file or runs a command." },
    { mode: "acceptEdits", label: "Auto-accept edits", note: "Claude edits files in the folder without asking. It still asks before running commands." },
    { mode: "plan", label: "Plan only", note: "Claude reads and plans but changes nothing until you approve its plan." },
    { mode: "bypassPermissions", label: "Bypass all permissions", note: "Claude runs every command and edit without asking you first." },
  ];
  export const BYPASS = "bypassPermissions";
  /** A mode's label; an unknown mode as the CLI spells it. */
  export const labelOf = (m) => MODES.find((x) => x.mode === m)?.label ?? String(m ?? "");
</script>

<script>
  let { value = "default", onpick, bypass = null, host = "", busy = false, label = "Permissions" } = $props();
  let confirming = $state(false);

  function choose(m) {
    if (busy || m === value) { confirming = false; return; }
    if (m === BYPASS) { if (bypass === true) confirming = true; return; }
    confirming = false;
    onpick?.(m);
  }
  function confirm() { confirming = false; onpick?.(BYPASS); }
  const off = $derived(bypass !== true);
  const why = $derived(
    bypass === false
      ? `Off on ${host}. To allow it, restart the host there with --allow‑bypass.`
      : `${host} doesn't say whether it allows this. Update plexus-agents there and restart it with --allow‑bypass to use it.`,
  );
</script>

<div class="sc-modes" data-surface="sc-modes" role="radiogroup" aria-label={label} aria-busy={busy || undefined}>
  {#each MODES as m (m.mode)}
    {@const danger = m.mode === BYPASS}
    {@const disabled = busy || (danger && off)}
    <button type="button" class="mode" role="radio" aria-checked={value === m.mode} data-danger={danger || undefined}
      data-pending={(danger && confirming) || undefined} {disabled} onclick={() => choose(m.mode)}>
      <span class="ring" aria-hidden="true"></span>
      <span class="words">
        <span class="name">{m.label}</span>
        <span class="note">{#if danger && off}{why}{:else}{m.note}{/if}</span>
      </span>
    </button>
  {/each}
</div>

{#if confirming}
  <div class="confirm" role="alertdialog" aria-label="Bypass all permissions?">
    <p class="head">Let Claude act without asking?</p>
    <p>Nothing will stop a command that deletes files, rewrites history or pushes code{host ? ` on ${host}` : ""}. You won't see a request; you'll only read what already ran. Use it for a folder you could lose.</p>
    <div class="actions">
      <button type="button" class="yes" onclick={confirm}>Bypass permissions</button>
      <button type="button" class="no" onclick={() => (confirming = false)}>Keep asking</button>
    </div>
  </div>
{/if}

<style>
  .sc-modes { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: var(--s2); }
  .mode {
    display: flex; align-items: flex-start; gap: var(--s3); min-height: var(--tap); min-width: 0;
    padding: var(--s3); text-align: left; font: inherit; color: var(--ink);
    background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius-sm);
    cursor: pointer; -webkit-tap-highlight-color: transparent;
    transition: background-color var(--motion-fast) var(--ease), border-color var(--motion-fast) var(--ease);
  }
  .mode:hover { background: var(--hover); }
  .mode:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
  .mode[aria-checked=true] { background: var(--sel); border-color: var(--accent); box-shadow: inset 0 0 0 1px var(--accent); }
  .ring { flex: none; width: 16px; height: 16px; margin-top: 2px; border: 1.5px solid var(--line-strong); border-radius: var(--radius-pill); background: var(--card); }
  [aria-checked=true] .ring { border-color: var(--accent); box-shadow: inset 0 0 0 4px var(--accent); }
  .words { display: grid; gap: 2px; min-width: 0; }
  .name { font-weight: var(--weight-medium); }
  .note { font-size: var(--text-note); line-height: 1.4; color: var(--muted); }

  /* The dangerous one: brick, and the only one that never looks like a default. */
  .mode[data-danger] .name { color: var(--danger); }
  .mode[data-danger] .ring { border-color: var(--danger); }
  .mode[data-danger]:hover:not(:disabled) { background: var(--danger-wash); }
  .mode[data-danger][aria-checked=true], .mode[data-danger][data-pending] { background: var(--danger-wash); border-color: var(--danger); box-shadow: inset 0 0 0 1px var(--danger); }
  .mode[data-danger][aria-checked=true] .ring { border-color: var(--danger); box-shadow: inset 0 0 0 4px var(--danger); }
  .mode:disabled { cursor: not-allowed; background: var(--bg); }
  .mode:disabled:hover { background: var(--bg); }
  .mode:disabled .name { color: var(--disabled); }
  .mode:disabled .ring { border-color: var(--line); }

  .confirm {
    display: grid; gap: var(--s2); margin-top: var(--s2); padding: var(--s3) var(--s4);
    background: var(--danger-wash); border: var(--border) solid var(--danger); border-left-width: 4px; border-radius: var(--radius-sm);
  }
  .confirm p { margin: 0; max-width: 64ch; line-height: var(--leading); }
  .confirm .head { font-weight: var(--weight-strong); color: var(--danger); }
  .actions { display: flex; flex-wrap: wrap; gap: var(--s2); margin-top: var(--s1); }
  .actions button {
    min-height: 36px; padding: 0 var(--s4); font: inherit; font-weight: var(--weight-medium);
    border-radius: var(--radius-sm); cursor: pointer; -webkit-tap-highlight-color: transparent;
  }
  .yes { color: var(--on-accent); background: var(--danger); border: var(--border) solid var(--danger); }
  .no { color: var(--ink); background: var(--card); border: var(--border) solid var(--line-strong); }
  .no:hover { background: var(--hover); }
  .actions button:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }

  @media (max-width: 719.98px) {
    .sc-modes { grid-template-columns: minmax(0, 1fr); }
    .actions { display: grid; grid-template-columns: 1fr 1fr; }
    .actions button { min-height: var(--tap); }
  }
</style>
