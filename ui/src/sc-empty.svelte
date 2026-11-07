<!-- stub (SC-31): SC-30 owns sc-empty. -->
<script module>
  export const uses = [];
</script>

<script>
  let { title = "", body = "", children } = $props();
</script>

<div class="sc-empty"><p class="t">{title}</p>{#if body}<p class="b">{body}</p>{/if}{@render children?.()}</div>

<style>
  .sc-empty { padding: var(--s5) var(--s4); color: var(--muted); }
  .t { margin: 0 0 var(--s1); color: var(--ink); font-weight: 600; }
  .b { margin: 0; }
</style>
