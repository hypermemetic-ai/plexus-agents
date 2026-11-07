<!-- SC-30: an empty or error state that says what to do. `title` the one line
     that names the situation, `body` what to do about it (plain words, never an
     apology), `children` the action — usually an sc-button or a command. Left
     aligned, in the flow of the page, no illustration. -->
<script module>
  export const uses = [];
</script>

<script>
  let { title = "", body = "", children = undefined } = $props();
</script>

<section class="sc-empty" data-surface="sc-empty">
  {#if title}<h2 class="title">{title}</h2>{/if}
  {#if body}<p class="body">{body}</p>{/if}
  {#if children}<div class="act">{@render children()}</div>{/if}
</section>

<style>
  .sc-empty {
    max-width: 56ch;
    padding: var(--s5) 0 var(--s5) var(--s4);
    border-left: 2px solid var(--line-strong);
    font-family: var(--font-text);
  }
  .title { margin: 0 0 var(--s2); font-size: var(--text-h3); font-weight: var(--weight-strong); line-height: 1.3; color: var(--ink); letter-spacing: -0.005em; }
  .body { margin: 0; font-size: var(--text-prose); line-height: var(--leading-prose); color: var(--muted); }
  .act { display: flex; flex-wrap: wrap; gap: var(--s2); margin-top: var(--s4); }
</style>
