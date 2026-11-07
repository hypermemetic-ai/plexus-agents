<!-- stub (SC-33): sc-empty {title, body, children} as the contract names it, until
     SC-30's lands: an empty or error state that says what to do. -->
<script module>
  export const uses = [];
</script>

<script>
  let { title = "", body = "", children } = $props();
</script>

<div class="sc-empty" data-surface="sc-empty">
  {#if title}<p class="title">{title}</p>{/if}
  {#if body}<p class="body">{body}</p>{/if}
  {#if children}{@render children()}{/if}
</div>

<style>
  .sc-empty { padding: var(--s4); color: var(--ink); }
  .title { margin: 0 0 var(--s2); font-weight: var(--weight-medium); }
  .body { margin: 0 0 var(--s3); color: var(--muted); }
</style>
