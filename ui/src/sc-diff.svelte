<!-- SC-31: an edit as an inline diff card (SUPERCONDUCTOR-27, CloudCLI's Edit/Write
     cards): the file, its +/− counts on the card's edge, and the changed lines
     under it, removed in the diff-del pair and added in the diff-add pair, three
     unchanged lines around each change. Open at once when it is short; a long
     one opens on a tap. Edit, MultiEdit and Write calls are read by `editOf`,
     which sc-toolrun and sc-turnfoot count with. Text only, never HTML. -->
<script module>
  export const uses = [];
  const CAP = 600; // lines a side the line diff compares; past it, all out and all in

  /** The line diff of two texts: [{ op: " " | "+" | "-", text }]. */
  export function lineDiff(a, b) {
    const x = a === "" ? [] : String(a ?? "").split("\n");
    const y = b === "" ? [] : String(b ?? "").split("\n");
    if (x.length > CAP || y.length > CAP) return [...x.map((text) => ({ op: "-", text })), ...y.map((text) => ({ op: "+", text }))];
    const n = x.length, m = y.length;
    const L = Array.from({ length: n + 1 }, () => new Uint16Array(m + 1));
    for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) L[i][j] = x[i] === y[j] ? L[i + 1][j + 1] + 1 : Math.max(L[i + 1][j], L[i][j + 1]);
    const out = [];
    let i = 0, j = 0;
    while (i < n && j < m) {
      if (x[i] === y[j]) { out.push({ op: " ", text: x[i] }); i++; j++; }
      else if (L[i + 1][j] >= L[i][j + 1]) out.push({ op: "-", text: x[i++] });
      else out.push({ op: "+", text: y[j++] });
    }
    while (i < n) out.push({ op: "-", text: x[i++] });
    while (j < m) out.push({ op: "+", text: y[j++] });
    return out;
  }

  /** An editing call as { path, lines, add, del, created }, or null for any other call. */
  export function editOf(name, input) {
    const i = input ?? {};
    let pairs;
    if (name === "Edit") pairs = [[i.old_string ?? "", i.new_string ?? ""]];
    else if (name === "MultiEdit") pairs = (i.edits ?? []).map((e) => [e.old_string ?? "", e.new_string ?? ""]);
    else if (name === "Write") pairs = [["", i.content ?? ""]];
    else return null;
    const lines = [];
    pairs.forEach(([a, b], k) => { if (k) lines.push({ op: "…", text: "" }); lines.push(...lineDiff(a, b)); });
    return {
      path: i.file_path ?? i.path ?? "",
      lines,
      add: lines.filter((l) => l.op === "+").length,
      del: lines.filter((l) => l.op === "-").length,
      created: name === "Write",
    };
  }

  /** `ctx` unchanged lines kept around each change; the rest fold to one gap row. */
  export function trim(lines, ctx = 3) {
    const near = (k) => lines.slice(Math.max(0, k - ctx), k + ctx + 1).some((o) => o.op === "+" || o.op === "-");
    const out = [];
    lines.forEach((l, k) => {
      if (l.op !== " " || near(k)) out.push(l);
      else if (out.length && out[out.length - 1].op !== "…") out.push({ op: "…", text: "" });
    });
    while (out.length && out[out.length - 1].op === "…") out.pop();
    return out;
  }

  /** A path relative to the thread's folder, when it is inside it. */
  export function relPath(path, folder) {
    const f = String(folder ?? "").replace(/\/$/, "");
    const p = String(path ?? "");
    return f && p.startsWith(f + "/") ? p.slice(f.length + 1) : p;
  }
</script>

<script>
  let { name, input, folder = "", failed = false, running = false } = $props();
  const edit = $derived(editOf(name, input));
  const shown = $derived(edit ? trim(edit.lines) : []);
  let open = $state(null);
  const isOpen = $derived(open ?? shown.length <= 16);
  const rel = $derived(edit ? relPath(edit.path, folder) : "");
  const verb = $derived(
    running ? (edit?.created ? "Writing" : "Editing")
    : failed ? (edit?.created ? "Could not write" : "Could not edit")
    : edit?.created ? "Wrote" : "Edited",
  );
</script>

{#if edit}
  <div class="sc-diff tool" data-failed={failed} data-running={running}>
    <button class="edge" type="button" aria-expanded={isOpen} onclick={() => (open = !isOpen)}>
      <span class="chev" aria-hidden="true">{isOpen ? "▾" : "▸"}</span>
      <span class="verb">{verb}</span>
      <span class="path" title={edit.path}>{rel}</span>
      <span class="counts"><span class="add">+{edit.add}</span> <span class="del">−{edit.del}</span></span>
    </button>
    {#if isOpen && shown.length}
      <div class="lines" aria-label={`Changes to ${rel}`}>
        {#each shown as l, k (k)}
          {#if l.op === "…"}
            <div class="gap" aria-label="unchanged lines"></div>
          {:else}
            <div class="ln" data-op={l.op}><span class="sign" aria-hidden="true">{l.op === "-" ? "−" : l.op === "+" ? "+" : ""}</span><span class="txt">{l.text || " "}</span></div>
          {/if}
        {/each}
      </div>
    {/if}
  </div>
{/if}

<style>
  .sc-diff { margin: var(--s1) 0; min-width: 0; overflow: hidden; background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-md); }
  .edge { display: flex; align-items: baseline; gap: var(--s2); width: 100%; min-height: 36px; padding: var(--s2) var(--s3); text-align: left; color: var(--ink); background: none; border: 0; cursor: pointer; }
  .edge:hover { background: var(--hover); }
  .edge:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }
  .chev { flex: none; width: 1em; color: var(--muted); }
  .verb { flex: none; color: var(--muted); }
  .path { min-width: 0; overflow-wrap: anywhere; font-family: var(--font-mono); font-size: 12.5px; }
  .counts { flex: none; margin-left: auto; font-family: var(--font-mono); font-size: 12px; }
  .add { color: var(--diff-add-fg); }
  .del { color: var(--diff-del-fg); }
  .sc-diff[data-failed=true] .verb { color: var(--st-failed-fg); }
  .lines { max-height: 420px; overflow: auto; padding: var(--s1) 0; border-top: 1px solid var(--line); font: 12px/1.6 var(--font-mono); }
  .ln { display: flex; min-width: max-content; }
  .sign { flex: none; width: 3ch; text-align: center; color: var(--muted); user-select: none; }
  .txt { padding-right: var(--s3); white-space: pre; }
  .ln[data-op="+"] { background: var(--diff-add-bg); }
  .ln[data-op="+"] .sign, .ln[data-op="+"] .txt { color: var(--diff-add-fg); }
  .ln[data-op="-"] { background: var(--diff-del-bg); }
  .ln[data-op="-"] .sign, .ln[data-op="-"] .txt { color: var(--diff-del-fg); }
  .gap { height: var(--s3); margin: var(--s1) 0; border-top: 1px dashed var(--line); border-bottom: 1px dashed var(--line); }
  @media (max-width: 719px) {
    .ln { min-width: 0; }
    .txt { white-space: pre-wrap; overflow-wrap: anywhere; }
  }
</style>
