<!-- stub: SC-30 owns sc-text; this stand-in draws text through the engine's markdown(). -->
<script module>
  export const uses = [];
</script>

<script>
  import { bus } from "plexus-ui/host";
  let { text = "" } = $props();
  const b = bus();
</script>

<div class="t">{@html b.markdown(String(text ?? ""))}</div>

<style>
  .t { max-width: 72ch; line-height: var(--leading); overflow-wrap: anywhere; }
  .t :global(p) { margin: 0 0 var(--s2); }
</style>
