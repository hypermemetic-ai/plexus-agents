<!-- stub (SC-31): SC-30 copies menger code-card in. {code, lang, wrap}. -->
<script module>
  export const uses = [];
</script>

<script>
  let { code = "", lang = "", wrap = false } = $props();
</script>

<figure class="code-card" data-wrap={wrap ? "on" : "off"}>
  <div class="edge">{lang || "text"}</div>
  <pre><code>{code}</code></pre>
</figure>

<style>
  .code-card { margin: var(--s2) 0; min-width: 0; max-width: 100%; padding: var(--s2) var(--s3); background: var(--panel); border: 1px solid var(--line); border-radius: var(--radius-md); }
  .edge { font-family: var(--font-mono); font-size: 11px; color: var(--muted); margin-bottom: var(--s1); }
  pre { margin: 0; overflow-x: auto; font: 12.5px/1.55 var(--font-mono); white-space: pre; }
  .code-card[data-wrap=on] pre { white-space: pre-wrap; overflow-wrap: anywhere; }
</style>
