<!-- SUPERCONDUCTOR-34: pairing a machine. `#/pair` explains how, shows the command
     to run there, and takes the code its terminal prints; `#/pair/:id` is the link
     it prints. Either way the page shows the code large and asks the one safety
     question: does your terminal show this code? Pair, or cancel. Main content only. -->
<script module>
  export const uses = ["auth.whoami", "hosts.pairing", "hosts.confirm"];
</script>

<script>
  import { bus, live } from "plexus-ui/host";
  import Button from "./sc-button.svelte";
  import Field from "./sc-field.svelte";
  import Empty from "./sc-empty.svelte";

  let { id = null } = $props();
  const b = bus();
  const door = live({ snapshot: "auth.whoami", params: {}, key: "principal", on: {} });
  const me = $derived(door.rows.find((r) => r.principal) ?? null);
  let code = $state("");
  let found = $state(null);
  let error = $state("");
  let paired = $state(null);
  let busy = $state(false);
  let copied = $state(false);

  const answer = (r, type) => {
    const ev = (r.events ?? []).find((e) => e.type === type);
    if (ev) return [ev, ""];
    const failed = (r.events ?? []).find((e) => e.type === "failed");
    return [null, failed ? failed.message : (r.message ?? "The server gave no answer.")];
  };
  async function find(by) {
    busy = true; error = "";
    const [p, why] = answer(await b.call("hosts.pairing", by), "pending");
    busy = false;
    found = p; error = why;
  }
  $effect(() => {
    found = null; paired = null; error = "";
    if (me && id) find({ pairing_id: id });
  });
  async function pair() {
    busy = true;
    const [done, why] = answer(await b.call("hosts.confirm", { pairing_id: found.pairing_id }), "paired");
    busy = false;
    paired = done; error = why;
  }
  function typed(ev) {
    ev.preventDefault();
    if (code.trim()) find({ code: code.trim().toUpperCase() });
  }
  function cancel() { found = null; error = ""; b.navigate("#/pair"); }
  const command = $derived(`plexus-agents host --server ${location.origin.replace(/^http/, "ws")}/ws --app ${location.origin} --name my-laptop --db ~/.plexus/agents-my-laptop.db`);
  async function copy() {
    try { await navigator.clipboard.writeText(command); copied = true; setTimeout(() => (copied = false), 1800); } catch { /* no clipboard here */ }
  }
</script>

<div class="sc-page">
  {#if door.status === "pending"}
    <p class="quiet">Loading…</p>
  {:else if !me}
    <Empty title="Sign in to pair a machine" body="Pairing makes the machine yours, so you have to be signed in to confirm it.">
      <p><a class="link-button" href={"/login?next=" + encodeURIComponent("/" + (location.hash || ""))}>Sign in with GitHub</a></p>
    </Empty>
  {:else if paired}
    <h1>{paired.name} is paired</h1>
    <p class="lede">It connects on its own from now on. Its threads show up in the sidebar.</p>
    <div class="actions">
      <Button kind="primary" onclick={() => b.navigate(`#/h/${encodeURIComponent(paired.name)}/new`)}>Start a thread on {paired.name}</Button>
      <Button onclick={() => b.navigate(`#/h/${encodeURIComponent(paired.name)}`)}>Open {paired.name}</Button>
    </div>
  {:else if found}
    <h1>Pair {found.name}?</h1>
    <p class="lede">Does your terminal show this code?</p>
    <div class="code" aria-label="pairing code">{found.code}</div>
    <p class="warn">Only pair if the codes match exactly. If your terminal shows anything else, someone else may be trying to connect: cancel.</p>
    <div class="actions">
      <Button kind="primary" disabled={busy} onclick={pair}>Pair</Button>
      <Button onclick={cancel}>Cancel</Button>
    </div>
    {#if error}<p class="error" role="alert">{error}</p>{/if}
  {:else if id && busy}
    <p class="quiet">Looking up the pairing…</p>
  {:else}
    <h1>Pair a machine</h1>
    <p class="lede">Pairing lets you run Claude on one of your computers from this page, from anywhere.</p>
    <ol class="steps">
      <li>
        <p>On the computer, run:</p>
        <div class="command">
          <code>{command}</code>
          <Button onclick={copy}>{copied ? "Copied" : "Copy"}</Button>
        </div>
        <p class="quiet">Swap <code class="inline">my-laptop</code> for a name you'll recognise.</p>
      </li>
      <li><p>It prints a link and a code. Open the link here, or type the code below.</p></li>
      <li><p>Check the code on this page matches your terminal, then pair.</p></li>
    </ol>
    <form class="enter" onsubmit={typed}>
      <Field label="Pairing code" {error}>
        <input class="code-input" value={code} oninput={(e) => (code = e.currentTarget.value)} autocomplete="off" autocapitalize="characters" spellcheck="false" placeholder="XXXX-XXXX" aria-label="pairing code"/>
      </Field>
      <Button kind="primary" type="submit" disabled={busy || !code.trim()}>Find machine</Button>
    </form>
  {/if}
</div>

<style>
  .sc-page { max-width: 680px; padding: var(--s6) var(--s5); }
  h1 { margin: 0 0 var(--s3); font-size: 24px; font-weight: 600; letter-spacing: -0.01em; overflow-wrap: anywhere; }
  p { margin: var(--s2) 0; }
  .lede { max-width: 60ch; }
  .quiet { color: var(--muted); }
  .error { color: var(--st-failed-fg); }
  .warn { max-width: 60ch; color: var(--muted); }
  .actions { display: flex; flex-wrap: wrap; gap: var(--s3); margin-top: var(--s4); }
  .code { display: inline-block; margin: var(--s3) 0; padding: var(--s4) var(--s5); font: 600 40px/1 var(--font-mono); letter-spacing: 0.12em; background: var(--panel); border: 2px solid var(--line); border-radius: var(--radius-md); overflow-wrap: anywhere; }

  .steps { margin: var(--s5) 0; padding-left: var(--s5); display: grid; gap: var(--s3); }
  .steps li::marker { color: var(--muted); font-weight: 600; }
  .command { display: flex; align-items: center; gap: var(--s3); margin: var(--s2) 0; padding: var(--s3) var(--s3) var(--s3) var(--s4); background: var(--panel); border: 1px solid var(--line); border-radius: var(--radius-md); }
  .command code { flex: 1; min-width: 0; font-family: var(--font-mono); font-size: 13px; overflow-wrap: anywhere; }
  code.inline { font-family: var(--font-mono); font-size: 13px; }

  .enter { display: flex; align-items: flex-end; flex-wrap: wrap; gap: var(--s3); padding-top: var(--s4); border-top: 1px solid var(--line); }
  .code-input { width: 14ch; min-height: 44px; padding: 0 var(--s3); font: 500 18px var(--font-mono); letter-spacing: 0.08em; text-transform: uppercase; color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); }
  .code-input:focus { outline: none; border-color: var(--focus); box-shadow: 0 0 0 1px var(--focus); }
  .link-button { display: inline-flex; align-items: center; min-height: 44px; padding: 0 var(--s4); color: var(--on-accent); background: var(--accent); border-radius: var(--radius-sm); font-weight: 500; }

  @media (max-width: 719px) {
    .sc-page { padding: var(--s4); }
    .code { font-size: 32px; display: block; text-align: center; }
    .actions { flex-direction: column; }
    .command { flex-direction: column; align-items: stretch; }
    .enter { flex-direction: column; align-items: stretch; }
    .code-input { width: 100%; }
  }
</style>
