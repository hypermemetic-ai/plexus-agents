<!-- SUPERCONDUCTOR-11: pairing a host. `#/pair/:id` is the link a host prints;
     `#/pair` takes the code its terminal shows. Either way the page shows the code
     large and asks: does your terminal show this code? Pair, or cancel. -->
<script module>
  export const uses = ["auth.whoami", "hosts.pairing", "hosts.confirm"];
</script>

<script>
  import { bus, live } from "plexus-ui/host";
  import Top from "./sc-top.svelte";
  import Nav from "./sc-nav.svelte";

  let { id = null } = $props();
  const b = bus();
  const door = live({ snapshot: "auth.whoami", params: {}, key: "principal", on: {} });
  const me = $derived(door.rows.find((r) => r.principal) ?? null);
  let code = $state("");
  let found = $state(null);
  let error = $state("");
  let paired = $state(null);
  let busy = $state(false);

  const answer = (r, type) => {
    const ev = (r.events ?? []).find((e) => e.type === type);
    if (ev) return [ev, ""];
    const failed = (r.events ?? []).find((e) => e.type === "failed");
    return [null, failed ? failed.message : (r.message ?? "no answer")];
  };
  async function find(by) {
    busy = true;
    error = "";
    const [p, why] = answer(await b.call("hosts.pairing", by), "pending");
    busy = false;
    found = p;
    error = why;
  }
  $effect(() => {
    found = null; paired = null; error = "";
    if (me && id) find({ pairing_id: id });
  });
  async function pair() {
    busy = true;
    const [done, why] = answer(await b.call("hosts.confirm", { pairing_id: found.pairing_id }), "paired");
    busy = false;
    paired = done;
    error = why;
  }
  function typed(ev) {
    ev.preventDefault();
    if (code.trim()) find({ code: code.trim().toUpperCase() });
  }
</script>

<div class="page">
  <Top/>
  <div class="body">
    <Nav/>
    <main>
      {#if door.status === "pending"}
        <p class="muted">…</p>
      {:else if !me}
        <h1>Pair a host</h1>
        <p>Sign in to confirm the pairing.</p>
        <p><a class="primary" href={"/login?next=" + encodeURIComponent("/" + (location.hash || ""))}>Sign in with GitHub</a></p>
      {:else if paired}
        <h1><code>{paired.name}</code> is yours</h1>
        <p>It goes on as itself now.</p>
        <p><a href={`#/h/${encodeURIComponent(paired.name)}`}>Open {paired.name}</a></p>
      {:else if found}
        <h1>Pair <code>{found.name}</code>?</h1>
        <p>Does your terminal show this code?</p>
        <div class="code">{found.code}</div>
        <p class="muted">If it shows anything else, someone is in the middle: don't pair.</p>
        <p class="actions">
          <button class="primary" type="button" disabled={busy} onclick={pair}>Pair</button>
          <a href="#/">Cancel</a>
        </p>
        {#if error}<p class="error">{error}</p>{/if}
      {:else}
        <h1>Pair a host</h1>
        <p>Type the code the host's terminal shows.</p>
        <form onsubmit={typed}>
          <input value={code} oninput={(e) => (code = e.currentTarget.value)} autocomplete="off" placeholder="XXXX-XXXX" size="12" aria-label="pairing code"/>
          <button class="primary" type="submit" disabled={busy}>Find</button>
        </form>
        {#if error}<p class="error">{error}</p>{/if}
      {/if}
    </main>
  </div>
</div>

<style>
  .page { display: grid; grid-template-rows: auto 1fr; height: 100vh; }
  .body { display: flex; min-height: 0; }
  main { flex: 1; overflow-y: auto; padding: 24px 32px; max-width: 760px; }
  h1 { font-size: 20px; margin: 0 0 16px; }
  .code { display: inline-block; margin: 8px 0 12px; padding: 14px 22px; font: var(--weight-bold) 40px/1 var(--font-text); letter-spacing: .12em; background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius-lg); }
  .actions { display: flex; gap: 16px; align-items: center; }
  .muted { color: var(--muted); }
  .error { color: var(--warn); }
  input { padding: 6px 10px; font-family: var(--font-text); letter-spacing: .08em; background: var(--bg); border: var(--border) solid var(--line); border-radius: var(--radius); }
  input:focus-visible { outline: none; border-color: var(--focus); }
  .primary { padding: 6px 16px; color: var(--on-accent); background: var(--accent); border: none; border-radius: var(--radius); cursor: pointer; }
  .primary:disabled { background: var(--disabled); cursor: default; }
  @media (max-width: 700px) { .body :global(.sc-nav) { display: none; } main { padding: 16px; } .actions .primary { width: 100%; } }
</style>
