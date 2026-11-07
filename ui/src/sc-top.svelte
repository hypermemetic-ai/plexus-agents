<!-- SUPERCONDUCTOR-11: the top bar on every page, after menger's app-top: the
     brand, pairing, and who is signed in (`auth.whoami`, a live one-shot read as
     app-top reads it), with sign-out (plexus-login's POST /logout) or a link to
     sign in (plexus-login's /login, coming back to this page). -->
<script module>
  export const uses = ["auth.whoami"];
</script>

<script>
  import { live } from "plexus-ui/host";

  const door = live({ snapshot: "auth.whoami", params: {}, key: "principal", on: {} });
  const me = $derived(door.rows.find((r) => r.principal) ?? null);
  const name = $derived(me ? (me.name || me.username || me.principal) : "");
  const back = $derived(encodeURIComponent("/" + (location.hash || "")));
</script>

<header class="sc-top" data-surface="sc-top">
  <a class="brand" href="#/">superconductor</a>
  <nav class="links" aria-label="pages">
    <a href="#/">hosts</a>
    <a href="#/pair">pair a host</a>
  </nav>
  <span class="who">
    {#if me}
      <span class="name" title={me.principal}>{name}</span>
      <form method="post" action="/logout"><button type="submit">sign out</button></form>
    {:else if door.status !== "pending"}
      <a class="signin" href={"/login?next=" + back}>sign in with GitHub</a>
    {/if}
  </span>
</header>

<style>
  .sc-top { display: flex; align-items: center; gap: 14px; height: 44px; padding: 0 14px; background: var(--panel); border-bottom: var(--border) solid var(--line); white-space: nowrap; }
  .brand { font-weight: var(--weight-bold); color: var(--ink); }
  .links { display: flex; gap: 4px; }
  .links a { padding: 4px 10px; color: var(--muted); border-radius: var(--radius); transition: background-color var(--motion-fast) var(--ease); }
  .links a:hover { color: var(--ink); background: var(--hover); }
  .who { margin-left: auto; display: flex; align-items: center; gap: 10px; color: var(--muted); }
  .who form { margin: 0; }
  .who button { padding: 3px 8px; background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius); cursor: pointer; }
  .signin { padding: 4px 10px; color: var(--on-accent); background: var(--accent); border-radius: var(--radius); }
  a:focus-visible, button:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
</style>
