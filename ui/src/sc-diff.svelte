<!-- stub: SC-31 owns sc-diff; this stand-in shows the old lines then the new. -->
<script module>
  export const uses = [];
</script>

<script>
  let { old = "", new: next = "", path = "" } = $props();
  const lines = $derived([
    ...String(old ?? "").split("\n").filter((l, i, a) => l || a.length > 1).map((t) => ({ k: "del", t })),
    ...String(next ?? "").split("\n").filter((l, i, a) => l || a.length > 1).map((t) => ({ k: "add", t })),
  ]);
</script>

<div class="diff" title={path}>
  {#each lines as l, i (i)}<div class="ln" data-k={l.k}><span class="sign">{l.k === "add" ? "+" : "−"}</span>{l.t}</div>{/each}
</div>

<style>
  .diff { overflow-x: auto; font: 13px/1.5 var(--font-mono); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); }
  .ln { padding: 0 var(--s3); white-space: pre; }
  .ln[data-k=add] { color: var(--diff-add-fg); background: var(--diff-add-bg); }
  .ln[data-k=del] { color: var(--diff-del-fg); background: var(--diff-del-bg); }
  .sign { display: inline-block; width: 2ch; user-select: none; }
</style>
