<!-- stub (SC-34 builder): the contract's sc-empty; SC-30's real one replaces it. -->
<script module>
  export const uses = [];
</script>

<script>
  let { title = "", body = "", children } = $props();
</script>

<section class="sc-empty">
  {#if title}<h2>{title}</h2>{/if}
  {#if body}<p>{body}</p>{/if}
  {@render children?.()}
</section>

<style>
  .sc-empty { display: grid; gap: var(--s3); padding: var(--s5); background: var(--panel); border: 1px solid var(--line); border-radius: var(--radius-md); }
  h2 { margin: 0; font-size: 18px; }
  p { margin: 0; color: var(--muted); max-width: 60ch; }
</style>
