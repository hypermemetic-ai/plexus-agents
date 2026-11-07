<!-- SUPERCONDUCTOR-11: `#/` — your hosts: online or when last seen, how each signs
     in, its page, and unpair (behind a confirm). Signed out, a link to sign in
     with GitHub. -->
<script module>
  export const uses = ["auth.whoami", "hosts.list", "hosts.unpair"];
</script>

<script>
  import { bus, live } from "plexus-ui/host";
  import Top from "./sc-top.svelte";
  import Nav from "./sc-nav.svelte";

  const b = bus();
  const door = live({ snapshot: "auth.whoami", params: {}, key: "principal", on: {} });
  const me = $derived(door.rows.find((r) => r.principal) ?? null);
  let hosts = $state(null);
  let said = $state("");

  async function load() {
    const r = await b.call("hosts.list", {});
    const listed = r.ok ? (r.events ?? []).find((e) => e.type === "hosts") : null;
    hosts = listed ? listed.hosts : [];
    if (!r.ok) said = r.message ?? "";
  }
  $effect(() => { if (me) load(); });

  async function unpair(name) {
    if (!confirm(`Unpair ${name}? Its key is refused from now on, and its socket closed.`)) return;
    const r = await b.call("hosts.unpair", { name });
    const failed = (r.events ?? []).find((e) => e.type === "failed");
    said = !r.ok ? (r.message ?? "") : failed ? failed.message : `${name} is unpaired.`;
    load();
  }
  const when = (secs) => (secs ? new Date(secs * 1000).toLocaleString() : "never");
</script>

<div class="page">
  <Top/>
  <div class="body">
    <Nav/>
    <main>
      {#if door.status === "pending"}
        <p class="muted">…</p>
      {:else if !me}
        <h1>Your hosts, wherever they are</h1>
        <p>Sign in to see your hosts and chat with the agents on them.</p>
        <p><a class="primary" href={"/login?next=" + encodeURIComponent("/" + (location.hash || ""))}>Sign in with GitHub</a></p>
      {:else}
        <h1>Your hosts</h1>
        {#if said}<p class="said">{said}</p>{/if}
        {#if hosts && hosts.length}
          <table>
            <tbody>
              {#each hosts as h (h.name)}
                <tr>
                  <td><span class="dot" data-online={h.online}></span><a href={`#/h/${encodeURIComponent(h.name)}`}>{h.name}</a></td>
                  <td class="muted">{h.online ? "online" : "last seen " + when(h.last_seen)}</td>
                  <td class="muted">{h.mode}</td>
                  <td><button type="button" onclick={() => unpair(h.name)}>Unpair</button></td>
                </tr>
              {/each}
            </tbody>
          </table>
        {:else if hosts}
          <p class="muted">None yet. On a machine, run <code>plexus-agents host --server {location.origin.replace(/^http/, "ws")}/ws --name NAME</code>, and open the link it prints.</p>
        {/if}
        <p><a href="#/pair">Pair a host by its code</a></p>
      {/if}
    </main>
  </div>
</div>

<style>
  .page { display: grid; grid-template-rows: auto 1fr; height: 100vh; }
  .body { display: flex; min-height: 0; }
  main { flex: 1; overflow-y: auto; padding: 24px 32px; max-width: 900px; }
  h1 { font-size: 20px; margin: 0 0 16px; }
  table { border-collapse: collapse; width: 100%; }
  td { padding: 8px 10px; border-bottom: var(--border) solid var(--line); }
  .dot { display: inline-block; width: 8px; height: 8px; margin-right: 8px; border-radius: 50%; background: var(--disabled); }
  .dot[data-online=true] { background: var(--live); }
  .muted { color: var(--muted); }
  .said { color: var(--muted); }
  button { padding: 3px 10px; background: var(--card); border: var(--border) solid var(--line); border-radius: var(--radius); cursor: pointer; }
  button:hover { border-color: var(--warn); color: var(--warn); }
  .primary { display: inline-block; padding: 6px 14px; color: var(--on-accent); background: var(--accent); border-radius: var(--radius); }
  code { font-family: var(--font-text); font-size: 12px; }
  @media (max-width: 700px) { .body :global(.sc-nav) { display: none; } main { padding: 16px; } }
</style>
