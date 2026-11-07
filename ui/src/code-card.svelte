<!-- stub: SC-30 copies menger's code-card; this stand-in draws the code plain. -->
<script module>
  export const uses = [];
</script>

<script>
  let { code = "", lang = "", wrap = false } = $props();
</script>

<pre class="cc" class:wrap data-lang={lang}><code>{code}</code></pre>

<style>
  .cc { margin: 0; padding: var(--s3); overflow-x: auto; font: 13px/1.5 var(--font-mono); color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); }
  .wrap { white-space: pre-wrap; overflow-wrap: anywhere; }
</style>
