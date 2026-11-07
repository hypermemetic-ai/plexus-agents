<!-- stub (SC-34 builder): the contract's sc-button so the pages compile and render;
     SC-30's real one replaces it at merge. -->
<script module>
  export const uses = [];
</script>

<script>
  let { kind = "quiet", type = "button", disabled = false, onclick, children } = $props();
</script>

<button class="sc-button" data-kind={kind} {type} {disabled} {onclick}>{@render children?.()}</button>

<style>
  .sc-button { display: inline-flex; align-items: center; justify-content: center; gap: var(--s2); min-height: 44px; padding: 0 var(--s4); font: inherit; font-weight: 500; color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); cursor: pointer; }
  .sc-button:hover:not(:disabled) { background: var(--hover); }
  .sc-button[data-kind=primary] { color: var(--on-accent); background: var(--accent); border-color: var(--accent); }
  .sc-button[data-kind=primary]:hover:not(:disabled) { background: var(--accent); }
  .sc-button[data-kind=danger] { color: var(--st-failed-fg); background: var(--st-failed-bg); border-color: var(--st-failed-bg); }
  .sc-button:disabled { color: var(--disabled); background: var(--hover); border-color: var(--line); cursor: default; }
  .sc-button:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
</style>
