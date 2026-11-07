<!-- stub (SC-34 builder): the contract's sc-field; SC-30's real one replaces it. -->
<script module>
  export const uses = [];
</script>

<script>
  let { label = "", hint = "", error = "", children } = $props();
</script>

<label class="sc-field">
  {#if label}<span class="label">{label}</span>{/if}
  {@render children?.()}
  {#if error}<span class="error">{error}</span>{:else if hint}<span class="hint">{hint}</span>{/if}
</label>

<style>
  .sc-field { display: grid; gap: var(--s1); }
  .label { font-weight: 500; color: var(--ink); }
  .hint { color: var(--muted); font-size: 13px; }
  .error { color: var(--st-failed-fg); font-size: 13px; }
</style>
