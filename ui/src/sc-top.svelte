<!-- SC-30: the top bar, sc-frame's `top` section. The app's name (home), on a
     phone away from home a back control to `#/` in its place, and who is signed
     in (`auth.whoami`, read once as menger's app-top reads it) with an account
     menu: Sign out (plexus-login's POST /logout), and on a test deployment, for an
     admin, "Act as…" (`#/people`) — or, signed out, Sign in with GitHub, coming
     back to this page. Pairing lives on the home page, not here. `here` is the
     frame's: "home" on `#/`, "page" anywhere else.

     Acting (plexus-login's people.*, test only): while this session acts as
     someone for an admin, a band in ink sits over the bar on every screen —
     "Acting as <name>", and Return to <admin> (people.unbecome). Amber is "needs
     you" alone, so the band is the system's own ink turned over: the one inverted
     surface in the app. Off test `people.whoami` answers
     `can_act: false` and nobody acts; then nothing of this shows. -->
<script module>
  export const uses = ["auth.whoami", "people.whoami", "people.unbecome"];
</script>

<script>
  import { bus, live } from "plexus-ui/host";

  let { here = "home" } = $props();
  const b = bus();
  const door = live({ snapshot: "auth.whoami", params: {}, key: "principal", on: {} });
  const me = $derived(door.rows.find((r) => r.principal) ?? null);

  // people.* exists on test alone: an error here just means "not offered".
  let people = $state(null);
  $effect(() => {
    if (!me) return;
    b.call("people.whoami", {}).then((r) => {
      people = (r.events ?? []).find((e) => e.type === "whoami") ?? null;
    }).catch(() => { people = null; });
  });
  const acting = $derived(!!people?.acting_as);
  const tail = (p) => (p || "").split(/[:/]/).pop();
  const name = $derived(me ? people?.name || (acting ? tail(people?.principal) : "") || me.name || me.username || me.login || tail(me.principal) || "you" : "");
  const admin = $derived(people?.signed_in_as_name || people?.signed_in_as || "you");
  const initial = $derived((name || "?").trim().charAt(0).toUpperCase());

  let returning = $state(false);
  let failed = $state("");
  async function giveBack() {
    returning = true; failed = "";
    const r = await b.call("people.unbecome", {});
    const done = (r.events ?? []).find((e) => e.type === "returned");
    if (done) { location.assign("/#/people"); location.reload(); return; }
    returning = false;
    failed = (r.events ?? []).find((e) => e.type === "refused")?.message ?? "The server didn't confirm. Try again.";
  }

  let open = $state(false);
  let menuEl = $state(null);
  $effect(() => {
    if (!open) return;
    const away = (e) => { if (menuEl && !menuEl.contains(e.target)) open = false; };
    const esc = (e) => { if (e.key === "Escape") open = false; };
    addEventListener("pointerdown", away);
    addEventListener("keydown", esc);
    return () => { removeEventListener("pointerdown", away); removeEventListener("keydown", esc); };
  });

  let hash = $state(typeof location === "undefined" ? "#/" : location.hash || "#/");
  $effect(() => {
    const moved = () => { hash = location.hash || "#/"; open = false; };
    addEventListener("hashchange", moved);
    return () => removeEventListener("hashchange", moved);
  });
  const next = $derived(encodeURIComponent("/" + hash));
</script>

<div class="sc-topwrap" data-surface="sc-top-wrap">
  {#if acting}
    <div class="acting" role="status" data-surface="sc-acting">
      <svg class="mask" viewBox="0 0 20 20" width="18" height="18" aria-hidden="true"><path d="M3 6.5c2.2-1 4.6-1.4 7-1.4s4.8.4 7 1.4c0 4.3-1.9 7.6-4.6 7.6-1.2 0-1.7-1.3-2.4-1.3s-1.2 1.3-2.4 1.3C4.9 14.1 3 10.8 3 6.5Z" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"/><circle cx="7.2" cy="8.9" r="1.1" fill="currentColor"/><circle cx="12.8" cy="8.9" r="1.1" fill="currentColor"/></svg>
      <span class="said"><span class="lead">Acting as</span> <strong>{name}</strong>{" "}<span class="for">for {admin}</span></span>
      {#if failed}<span class="failed" role="alert">{failed}</span>{/if}
      <button class="return" type="button" disabled={returning} onclick={giveBack}>{returning ? "Returning…" : `Return to ${admin}`}</button>
    </div>
  {/if}
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
        <div class="account" bind:this={menuEl}>
          <button class="me" type="button" aria-haspopup="menu" aria-expanded={open} title={me.principal} onclick={() => (open = !open)}>
            <span class="avatar" data-acting={acting ? "" : undefined} aria-hidden="true">{initial}</span><span class="name">{name}</span>
            <svg class="caret" viewBox="0 0 12 12" width="12" height="12" aria-hidden="true"><path d="M3 4.5 6 7.5 9 4.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"/></svg>
          </button>
          {#if open}
            <div class="menu" role="menu" data-surface="sc-account-menu">
              <p class="whois"><span class="whoname">{name}</span><span class="whop">{me.principal}</span></p>
              {#if people?.can_act}
                <a class="item" role="menuitem" href="#/people">Act as…</a>
              {/if}
              {#if acting}
                <button class="item" role="menuitem" type="button" onclick={giveBack}>Return to {admin}</button>
              {/if}
              <form method="post" action="/logout"><button class="item" role="menuitem" type="submit">Sign out</button></form>
            </div>
          {/if}
        </div>
      {:else if door.status !== "pending"}
        <a class="signin" href={"/login?next=" + next}>Sign in with GitHub</a>
      {/if}
    </div>
  </header>
</div>

<style>
  .sc-topwrap { display: block; width: 100%; max-width: 100vw; }

  /* Acting: the system's ink turned over — the one inverted surface. */
  .acting {
    display: flex;
    align-items: center;
    gap: var(--s3);
    min-height: 40px;
    padding: calc(env(safe-area-inset-top) + var(--s1)) calc(var(--s4) + env(safe-area-inset-right)) var(--s1) calc(var(--s4) + env(safe-area-inset-left));
    background: var(--ink);
    color: var(--bg);
    font-family: var(--font-text);
    font-size: var(--text-size);
    line-height: 1.3;
  }
  .mask { flex: none; }
  .said { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .said strong { font-weight: var(--weight-strong); }
  .for { opacity: .72; }
  .failed { font-size: var(--text-note); }
  .return {
    margin-left: auto;
    flex: none;
    display: inline-flex;
    align-items: center;
    min-height: 30px;
    padding: 0 var(--s3);
    font: inherit;
    font-weight: var(--weight-medium);
    color: var(--ink);
    background: var(--bg);
    border: 0;
    border-radius: var(--radius-sm);
    cursor: pointer;
  }
  .return:hover { background: var(--panel); }
  .return:disabled { cursor: progress; opacity: .8; }
  .return:focus-visible { outline: 2px solid var(--bg); outline-offset: 2px; }

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
  }
  .acting + .sc-top { height: var(--topbar); padding-top: 0; }
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
  .account { position: relative; min-width: 0; }
  .me {
    display: inline-flex;
    align-items: center;
    gap: var(--s2);
    min-width: 0;
    min-height: 36px;
    padding: 0 var(--s2) 0 var(--s1);
    color: var(--ink);
    background: none;
    border: 0;
    border-radius: var(--radius-sm);
    font: inherit;
    cursor: pointer;
  }
  .me:hover, .me[aria-expanded=true] { background: var(--hover); }
  .caret { color: var(--muted); flex: none; }
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
  .avatar[data-acting] { background: var(--ink); color: var(--bg); border-color: var(--ink); }
  .name { overflow: hidden; text-overflow: ellipsis; max-width: 22ch; font-weight: var(--weight-medium); }
  .menu {
    position: absolute;
    right: 0;
    top: calc(100% + var(--s1));
    z-index: 30;
    min-width: 220px;
    max-width: min(320px, calc(100vw - 2 * var(--s4)));
    padding: var(--s1);
    background: var(--card);
    border: var(--border) solid var(--line);
    border-radius: var(--radius-md);
    box-shadow: 0 8px 24px var(--shadow);
    white-space: normal;
  }
  .whois { display: grid; gap: 2px; margin: 0; padding: var(--s2) var(--s3) var(--s3); border-bottom: var(--border) solid var(--line); margin-bottom: var(--s1); }
  .whoname { font-weight: var(--weight-medium); color: var(--ink); }
  .whop { font-family: var(--font-mono); font-size: var(--text-small); color: var(--muted); overflow-wrap: anywhere; }
  .item {
    display: flex;
    align-items: center;
    width: 100%;
    min-height: 36px;
    padding: 0 var(--s3);
    color: var(--ink);
    background: none;
    border: 0;
    border-radius: var(--radius-sm);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .item:hover { background: var(--hover); }
  .signin, .back {
    display: inline-flex;
    align-items: center;
    min-height: 32px;
    padding: 0 var(--s3);
    border-radius: var(--radius-sm);
    font-size: var(--text-note);
    cursor: pointer;
    transition: background-color var(--motion-fast) var(--ease), color var(--motion-fast) var(--ease);
  }
  .signin { color: var(--on-accent); background: var(--accent); font-weight: var(--weight-medium); }
  .signin:hover { background: var(--accent-hover); }
  .back { display: none; }
  a:focus-visible, button:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }

  @media (max-width: 719.98px) {
    .acting { padding-left: calc(var(--s4) + env(safe-area-inset-left)); padding-right: calc(var(--s2) + env(safe-area-inset-right)); gap: var(--s2); }
    .for { display: none; }
    .return { min-height: 36px; }
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
    .me { min-height: var(--tap); padding: 0 var(--s2); }
    .item { min-height: var(--tap); }
    .signin { min-height: var(--tap); }
  }
</style>
