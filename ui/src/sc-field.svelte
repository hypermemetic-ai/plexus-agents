<!-- SC-30: a field — its label over the control it wraps (an input, a
     textarea, a select: `children`), a hint under it, or in its place the
     error that says what to fix. The label is a <label> around everything, so a
     tap on the words focuses the control; the hint and error are announced
     with it. The wrapped control is styled here: 44px tall on a phone (iOS
     focus-zoom is prevented by index.html's viewport, not by a bigger font). -->
<script module>
  export const uses = [];
</script>

<script>
  let { label = "", hint = "", error = "", children = undefined } = $props();
</script>

<label class="sc-field" data-surface="sc-field" data-error={error ? "" : undefined}>
  {#if label}<span class="label">{label}</span>{/if}
  <span class="control">{@render children?.()}</span>
  {#if error}<span class="error" role="alert">{error}</span>{:else if hint}<span class="hint">{hint}</span>{/if}
</label>

<style>
  .sc-field { display: flex; flex-direction: column; gap: 6px; min-width: 0; }
  .label { font-size: var(--text-note); font-weight: var(--weight-medium); color: var(--ink); }
  .control { display: flex; min-width: 0; }
  .control :global(input), .control :global(select), .control :global(textarea) {
    flex: 1;
    min-width: 0;
    width: 100%;
    min-height: 38px;
    padding: 7px 11px;
    font-family: var(--font-text);
    font-size: var(--text-size);
    line-height: var(--leading);
    color: var(--ink);
    background: var(--card);
    border: var(--border) solid var(--line-strong);
    border-radius: var(--radius-sm);
    transition: border-color var(--motion-fast) var(--ease);
  }
  .control :global(textarea) { resize: vertical; min-height: 88px; }
  .control :global(input::placeholder), .control :global(textarea::placeholder) { color: var(--disabled); }
  .control :global(input:hover), .control :global(select:hover), .control :global(textarea:hover) { border-color: var(--muted); }
  .control :global(input:focus-visible), .control :global(select:focus-visible), .control :global(textarea:focus-visible) {
    outline: 2px solid var(--focus);
    outline-offset: -1px;
    border-color: var(--focus);
  }
  .control :global(:disabled) { color: var(--disabled); background: var(--bg); }
  .sc-field[data-error] .control :global(input), .sc-field[data-error] .control :global(select), .sc-field[data-error] .control :global(textarea) { border-color: var(--danger); }
  .hint { font-size: var(--text-note); color: var(--muted); }
  .error { font-size: var(--text-note); color: var(--danger); }
  @media (max-width: 719.98px) {
    .control :global(input), .control :global(select), .control :global(textarea) { min-height: var(--tap); }
  }
</style>
