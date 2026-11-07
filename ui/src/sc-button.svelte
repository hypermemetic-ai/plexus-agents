<!-- stub (SC-31): SC-30 owns sc-button. -->
<script module>
  export const uses = [];
</script>

<script>
  let { kind = "quiet", type = "button", disabled = false, onclick, children } = $props();
</script>

<button class="sc-button" data-kind={kind} {type} {disabled} {onclick}>{@render children?.()}</button>

<style>
  .sc-button { min-height: 32px; padding: 0 var(--s3); color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); cursor: pointer; }
  .sc-button[data-kind=primary] { color: var(--on-accent); background: var(--accent); border-color: var(--accent); }
  .sc-button[data-kind=danger] { color: var(--st-failed-fg); border-color: var(--st-failed-fg); }
  .sc-button:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
  .sc-button:disabled { color: var(--disabled); cursor: default; }
</style>
