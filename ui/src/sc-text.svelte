<!-- stub (SC-31): SC-30 owns sc-text. Text through markdown(), fences through code-card. -->
<script module>
  export const uses = [];
</script>

<script>
  import { markdown } from "plexus-ui/host";
  import CodeCard from "./code-card.svelte";
  let { text = "" } = $props();
  const parts = $derived.by(() => {
    const out = [];
    const re = /^```([^\n]*)\n([\s\S]*?)(?:^```\s*$|$(?![\s\S]))/gm;
    let at = 0, m;
    const src = String(text ?? "");
    while ((m = re.exec(src))) {
      if (m.index > at) out.push({ md: src.slice(at, m.index) });
      out.push({ code: m[2].replace(/\n$/, ""), lang: m[1].trim() });
      at = re.lastIndex;
      if (m[0].length === 0) re.lastIndex++;
    }
    if (at < src.length) out.push({ md: src.slice(at) });
    return out;
  });
</script>

<div class="sc-text">
  {#each parts as p, i (i)}
    {#if p.md != null}{@html markdown(p.md)}{:else}<CodeCard code={p.code} lang={p.lang}/>{/if}
  {/each}
</div>

<style>
  .sc-text { line-height: var(--leading); overflow-wrap: anywhere; }
  .sc-text :global(p) { margin: 0 0 var(--s3); }
  .sc-text :global(p:last-child) { margin-bottom: 0; }
  .sc-text :global(ul), .sc-text :global(ol) { margin: 0 0 var(--s3); padding-left: var(--s5); }
  .sc-text :global(code) { font-family: var(--font-mono); font-size: .9em; padding: 0 3px; background: var(--hover); border-radius: var(--radius-xs); }
  .sc-text :global(h1), .sc-text :global(h2), .sc-text :global(h3) { margin: var(--s4) 0 var(--s2); font-size: 1.05em; font-weight: 600; }
  .sc-text :global(a) { color: var(--accent); }
  .sc-text :global(table) { border-collapse: collapse; margin: 0 0 var(--s3); display: block; overflow-x: auto; }
  .sc-text :global(td), .sc-text :global(th) { padding: 2px var(--s2); border: 1px solid var(--line); }
</style>
