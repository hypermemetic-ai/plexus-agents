<!-- SC-30: the top bar, sc-frame's `top` section. The app's name (home), on a
     phone away from home a back control to `#/` in its place, and who is signed
     in (`auth.whoami`, read once as menger's app-top reads it) with Sign out
     (plexus-login's POST /logout) — or, signed out, Sign in with GitHub, coming
     back to this page. Pairing lives on the home page, not here. `here` is the
     frame's: "home" on `#/`, "page" anywhere else. -->
<script module>
  export const uses = ["auth.whoami"];
</script>

<script>
  import { live } from "plexus-ui/host";

  let { here = "home" } = $props();
  const door = live({ snapshot: "auth.whoami", params: {}, key: "principal", on: {} });
  const me = $derived(door.rows.find((r) => r.principal) ?? null);
  const name = $derived(me ? me.name || me.username || me.login || "you" : "");
  const initial = $derived((name || "?").trim().charAt(0).toUpperCase());

  let hash = $state(typeof location === "undefined" ? "#/" : location.hash || "#/");
  $effect(() => {
    const moved = () => (hash = location.hash || "#/");
    addEventListener("hashchange", moved);
    return () => removeEventListener("hashchange", moved);
  });
  const next = $derived(encodeURIComponent("/" + hash));
</script>

<header class="sc-top" data-surface="sc-top" data-here={here}>
  <a class="back" href="#/" aria-label="Back to all threads">
    <svg viewBox="0 0 20 20" width="20" height="20" aria-hidden="true"><path d="M12.5 4.5 7 10l5.5 5.5" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>
    <span>Threads</span>
  </a>
  <a class="brand" href="#/">
    <svg class="mark" viewBox="0 0 20 20" width="18" height="18" aria-hidden="true"><path d="M2 7h7a3 3 0 0 1 3 3v0a3 3 0 0 0 3 3h3M2 13h4M14 7h4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"/></svg>
    <span>superconductor</span>
  </a>
  <div class="who">
    {#if me}
      <span class="me" title={me.principal}><span class="avatar" aria-hidden="true">{initial}</span><span class="name">{name}</span></span>
      <form method="post" action="/logout"><button class="out" type="submit">Sign out</button></form>
    {:else if door.status !== "pending"}
      <a class="signin" href={"/login?next=" + next}>Sign in with GitHub</a>
    {/if}
  </div>
</header>

<style>
  .sc-top {
    display: flex;
    align-items: center;
    gap: var(--s3);
    height: calc(var(--topbar) + env(safe-area-inset-top));
    padding: env(safe-area-inset-top) calc(var(--s4) + env(safe-area-inset-right)) 0 calc(var(--s4) + env(safe-area-inset-left));
    background: var(--panel);
    border-bottom: var(--border) solid var(--line);
    white-space: nowrap;
    font-family: var(--font-text);
    width: 100%;
    max-width: 100vw;
    min-width: 0;
    overflow: hidden;
  }
  .brand {
    display: inline-flex;
    align-items: center;
    gap: var(--s2);
    min-height: var(--tap);
    color: var(--ink);
    font-size: 15px;
    font-weight: var(--weight-strong);
    letter-spacing: -0.01em;
    min-width: 0;
    overflow: hidden;
  }
  .brand span { overflow: hidden; text-overflow: ellipsis; }
  .mark { color: var(--accent); flex: none; }
  .who { margin-left: auto; display: flex; align-items: center; gap: var(--s2); min-width: 0; }
  .who form { margin: 0; }
  .me { display: inline-flex; align-items: center; gap: var(--s2); min-width: 0; color: var(--ink); }
  .avatar {
    display: inline-grid;
    place-items: center;
    width: 26px;
    height: 26px;
    flex: none;
    border-radius: var(--radius-pill);
    background: var(--card);
    border: var(--border) solid var(--line-strong);
    color: var(--muted);
    font-size: var(--text-small);
    font-weight: var(--weight-strong);
  }
  .name { overflow: hidden; text-overflow: ellipsis; max-width: 22ch; font-weight: var(--weight-medium); }
  .out, .signin, .back {
    display: inline-flex;
    align-items: center;
    min-height: 32px;
    padding: 0 var(--s3);
    border-radius: var(--radius-sm);
    font-size: var(--text-note);
    cursor: pointer;
    transition: background-color var(--motion-fast) var(--ease), color var(--motion-fast) var(--ease);
  }
  .out { color: var(--muted); background: none; border: 0; }
  .out:hover { color: var(--ink); background: var(--hover); }
  .signin { color: var(--on-accent); background: var(--accent); font-weight: var(--weight-medium); }
  .signin:hover { background: var(--accent-hover); }
  .back { display: none; }
  a:focus-visible, button:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }

  @media (max-width: 719.98px) {
    .sc-top { padding-left: calc(var(--s2) + env(safe-area-inset-left)); padding-right: calc(var(--s2) + env(safe-area-inset-right)); gap: var(--s1); }
    .brand { padding-left: var(--s2); }
    .sc-top[data-here=page] .brand { display: none; }
    .sc-top[data-here=page] .back {
      display: inline-flex;
      gap: 2px;
      min-height: var(--tap);
      padding: 0 var(--s3) 0 var(--s1);
      color: var(--accent);
      font-size: var(--text-size);
      font-weight: var(--weight-medium);
    }
    .back:hover { background: var(--hover); }
    .name { display: none; }
    .out, .signin { min-height: var(--tap); }
  }
</style>
