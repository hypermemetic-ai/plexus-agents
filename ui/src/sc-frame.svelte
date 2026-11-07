<!-- SC-30: the one layout. Every route names it (routes.json); it draws four
     sections — `top` (sc-top), `nav` (sc-nav), `palette` (sc-palette), their
     defaults in slots.json and swappable from the section menu — and `main`, a
     bound mark whose component is the route's page, chosen here from the route:
     a thread → sc-thread, `#/h/:host/new` → sc-newthread, a host → sc-host, `#/pair[/:id]` → sc-pair, `#/people` → sc-people
     (test only: an admin acts as someone), else
     sc-home. The route's params reach the page as props through the mark's
     data-* (`host`, `thread`, `id`, as strings), and follow it on update.

     Desktop: the top bar over a 272px rail and the page. Phone (< 720px): one
     column; on `#/` the rail is the screen, home's content under it; elsewhere
     the page alone, and sc-top shows a back control (it reads `here`).

     Transitional: a page that still draws its own Top and Nav (written before
     SC-30) is noticed, and the frame then draws neither of its own, so the page
     is never doubled; the page drawn whole, the frame steps aside. -->
<script module>
  export const uses = [];
</script>

<script>
  let { host = null, thread = null, id = null } = $props();

  let hash = $state(typeof location === "undefined" ? "#/" : location.hash || "#/");
  $effect(() => {
    const moved = () => (hash = location.hash || "#/");
    addEventListener("hashchange", moved);
    return () => removeEventListener("hashchange", moved);
  });

  const page = $derived(
    thread ? "sc-thread" : host && /\/new$/.test(hash) ? "sc-newthread" : host ? "sc-host" : /^#\/pair(\/|$)/.test(hash) ? "sc-pair" : /^#\/people$/.test(hash) ? "sc-people" : "sc-home",
  );
  const here = $derived(page === "sc-home" ? "home" : "page");

  // A page that draws its own top bar or rail (before SC-30) has the screen to itself.
  let mainEl = $state(null);
  let whole = $state(false);
  $effect(() => {
    if (!mainEl) return;
    const look = () => { whole = !!mainEl.querySelector(".sc-top, .sc-nav"); };
    look();
    const watch = new MutationObserver(look);
    watch.observe(mainEl, { childList: true, subtree: true });
    return () => watch.disconnect();
  });
</script>

<div class="sc-frame" data-surface="sc-frame" data-here={here} data-whole={whole ? "" : undefined}>
  {#if !whole}
    <div class="top" data-slot="top" data-here={here}></div>
  {/if}
  <div class="body">
    {#if !whole}
      <nav class="nav" aria-label="Hosts and threads" data-slot="nav" data-here={here}></nav>
    {/if}
    <main class="main" bind:this={mainEl}>
      <div class="page" data-slot="main" data-component={page} data-host={host} data-thread={thread} data-id={id}></div>
    </main>
  </div>
  <div class="palette" data-slot="palette"></div>
</div>

<style>
  .sc-frame {
    position: fixed;
    inset: 0;
    overflow: hidden;
    display: flex;
    flex-direction: column;
    background: var(--bg);
    color: var(--ink);
    font-family: var(--font-text);
    font-size: var(--text-size);
    line-height: var(--leading);
  }
  .top { flex: none; }
  .top > :global(*) { display: block; }
  .body { flex: 1; min-height: 0; display: flex; }

  .nav {
    flex: none;
    width: var(--rail);
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    overscroll-behavior: contain;
    background: var(--panel);
    border-right: var(--border) solid var(--line);
    padding-left: env(safe-area-inset-left);
  }
  /* What fills the rail fills it; a nav written before SC-30 sets its own width and height. */
  .sc-frame .nav > :global(*) { flex: 1 0 auto; width: auto; max-width: 100%; height: auto; min-height: 100%; border-right: 0; }

  .main {
    flex: 1;
    min-width: 0;
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow-y: auto;
    overflow-x: hidden;
    padding-right: env(safe-area-inset-right);
  }
  .page { flex: 1; min-height: 0; min-width: 0; display: flex; flex-direction: column; }
  /* The page fills main and scrolls itself unless it pins its own parts (a composer). */
  .page > :global(*) { flex: 1 1 auto; min-height: 0; min-width: 0; }
  /* The element a page left behind when the route changed page (the host keeps it, empty). */
  .page > :global(:empty) { display: none; }

  /* The palette draws itself over the page (fixed); its mark takes no room. */
  .palette { flex: none; height: 0; overflow: visible; }

  /* A page drawn whole (its own Top and Nav): it has the screen. */
  .sc-frame[data-whole] .main { padding: 0; overflow: hidden; }
  .sc-frame[data-whole] .page > :global(*) { height: 100%; }

  @media (max-width: 719.98px) {
    .sc-frame { --gutter: 16px; --pitch: 44px; }
    .main { padding-right: 0; padding-bottom: env(safe-area-inset-bottom); }
    .nav { border-right: 0; padding-left: 0; }
    /* Home: the rail is the screen, home's own content under it, one scroll. */
    .sc-frame[data-here=home] .body { display: block; overflow-y: auto; overscroll-behavior: contain; }
    .sc-frame[data-here=home] .nav { --side: auto; --rail: auto; width: auto; overflow: visible; border-bottom: var(--border) solid var(--line); }
    .sc-frame[data-here=home] .nav > :global(*) { min-height: 0; overflow: visible; }
    .sc-frame[data-here=home] .main { overflow: visible; }
    /* Any other route: the page alone; sc-top carries the way back. */
    .sc-frame[data-here=page] .nav { display: none; }
  }
</style>
