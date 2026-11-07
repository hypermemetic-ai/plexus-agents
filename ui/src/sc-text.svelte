<!-- SC-30: words — the agent's or yours — as markdown. The only HTML this bundle
     draws from text goes through plexus-ui's `markdown()` (the engine's
     sanitized render); nothing else is ever {@html}. Over what it draws, each
     fenced block becomes a code-card in the fence's language (painted by its
     code-* component), a ```mermaid fence a mermaid-diagram, a wide table
     scrolls inside itself, and a link that leaves the app opens a new tab.
     Prose is set at most 72ch wide. -->
<script module>
  export const uses = [];
</script>

<script>
  import { mount, unmount } from "svelte";
  import { markdown } from "plexus-ui/host";
  import Diagram from "plexus-ui:mermaid-diagram";
  import CodeCard from "./code-card.svelte";

  let { text = "" } = $props();
  const html = $derived(markdown(String(text ?? "")));

  /** Draws the cards over the fences markdown() wrote; again whenever the html changes. */
  function cards(node, _html) {
    let placed = [];
    const clear = () => { for (const p of placed) { try { unmount(p.app); } catch { /* gone */ } p.at.remove(); } placed = []; };
    const place = () => {
      clear();
      for (const code of node.querySelectorAll("pre > code")) {
        const pre = code.parentElement;
        const lang = [...code.classList].find((c) => c.startsWith("language-"))?.slice("language-".length) ?? "";
        const at = document.createElement("div");
        pre.before(at);
        pre.hidden = true;
        const source = code.textContent.replace(/\n$/, "");
        const app = lang === "mermaid"
          ? mount(Diagram, { target: at, props: { source } })
          : mount(CodeCard, { target: at, props: { code: source, lang } });
        placed.push({ at, app });
      }
      for (const table of node.querySelectorAll("table")) {
        if (table.parentElement?.classList.contains("table-scroll")) continue;
        const wrap = document.createElement("div");
        wrap.className = "table-scroll";
        table.before(wrap);
        wrap.append(table);
      }
      for (const a of node.querySelectorAll("a[href]")) {
        const href = a.getAttribute("href") ?? "";
        if (/^https?:\/\//i.test(href) && !href.startsWith(location.origin)) { a.target = "_blank"; a.rel = "noopener noreferrer"; }
      }
    };
    place();
    return { update: place, destroy: clear };
  }
</script>

<div class="sc-text" data-surface="sc-text" use:cards={html}>{@html html}</div>

<style>
  .sc-text {
    max-width: var(--measure);
    min-width: 0;
    font-size: var(--text-prose);
    line-height: var(--leading-prose);
    color: var(--ink);
    overflow-wrap: anywhere;
  }
  .sc-text > :global(:first-child) { margin-top: 0; }
  .sc-text > :global(:last-child) { margin-bottom: 0; }
  .sc-text :global(p), .sc-text :global(ul), .sc-text :global(ol), .sc-text :global(blockquote) { margin: 0 0 var(--s3); }
  .sc-text :global(ul), .sc-text :global(ol) { padding-left: 1.4em; }
  .sc-text :global(li) { margin: 2px 0; }
  .sc-text :global(li > p) { margin: 0; }
  .sc-text :global(h1), .sc-text :global(h2), .sc-text :global(h3), .sc-text :global(h4) {
    margin: var(--s5) 0 var(--s2);
    font-weight: var(--weight-strong);
    line-height: 1.3;
    letter-spacing: -0.005em;
  }
  .sc-text :global(h1) { font-size: var(--text-h2); }
  .sc-text :global(h2) { font-size: var(--text-h3); }
  .sc-text :global(h3), .sc-text :global(h4) { font-size: var(--text-prose); }
  .sc-text :global(a) { color: var(--accent); text-decoration: underline; text-decoration-thickness: 1px; text-underline-offset: 2px; }
  .sc-text :global(a:focus-visible) { outline: 2px solid var(--focus); outline-offset: 2px; border-radius: var(--radius-xs); }
  .sc-text :global(strong) { font-weight: var(--weight-strong); }
  .sc-text :global(:not(pre) > code) {
    padding: 1px 5px;
    font-family: var(--font-mono);
    font-size: 0.88em;
    background: var(--panel);
    border: var(--border) solid var(--line);
    border-radius: var(--radius-xs);
  }
  /* A fence markdown() drew is hidden once its code-card is over it (code-card's own <pre> is its own). */
  .sc-text :global(pre[hidden]) { display: none; }
  .sc-text :global(blockquote) { padding-left: var(--s3); border-left: 3px solid var(--line-strong); color: var(--muted); }
  .sc-text :global(hr) { margin: var(--s5) 0; border: 0; border-top: var(--border) solid var(--line); }
  .sc-text :global(.table-scroll) { margin: 0 0 var(--s3); overflow-x: auto; max-width: 100%; }
  .sc-text :global(table) { border-collapse: collapse; font-size: var(--text-size); }
  .sc-text :global(th), .sc-text :global(td) { padding: 6px 10px; border-bottom: var(--border) solid var(--line); text-align: left; vertical-align: top; }
  .sc-text :global(th) { font-weight: var(--weight-strong); border-bottom-color: var(--line-strong); }
  .sc-text :global(img) { max-width: 100%; height: auto; }
</style>
