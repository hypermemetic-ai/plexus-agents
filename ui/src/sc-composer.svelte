<!-- stub (SC-31): SC-32 owns sc-composer. {host, thread, status}: send, queue, stop, Esc. -->
<script module>
  export const uses = ["hosts.send", "hosts.interrupt"];
</script>

<script>
  import { bus } from "plexus-ui/host";
  let { host, thread, status = "idle" } = $props();
  const b = bus();
  let text = $state("");
  let error = $state("");
  const running = $derived(status === "working");
  async function submit(ev) {
    ev?.preventDefault();
    const prompt = text.trim();
    if (!prompt) return;
    text = "";
    const r = await b.call("hosts.send", { host, thread, prompt });
    const f = (r.events ?? []).find((e) => e.type === "failed");
    if (!r.ok || f) { error = f ? f.message : (r.message ?? "not sent"); text = prompt; }
  }
  async function stop() { await b.call("hosts.interrupt", { host, thread }); }
  b.keys({ escape: { to: () => { if (running) stop(); }, label: "Stop the agent", in_inputs: true } });
  function keydown(ev) { if (ev.key === "Enter" && !ev.shiftKey && !ev.isComposing) { ev.preventDefault(); submit(); } }
</script>

<form class="sc-composer" onsubmit={submit}>
  <textarea rows="2" bind:value={text} onkeydown={keydown} placeholder="Reply to Claude"></textarea>
  {#if running}<button type="button" class="stop" onclick={stop}>Stop</button>{/if}
  <button type="submit" class="send">Send</button>
  {#if error}<p class="err">{error}</p>{/if}
</form>

<style>
  .sc-composer { display: flex; flex-wrap: wrap; gap: var(--s2); align-items: flex-end; }
  textarea { flex: 1; min-width: 0; padding: var(--s2) var(--s3); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-md); resize: none; }
  button { min-height: 44px; padding: 0 var(--s4); border: 1px solid var(--line); border-radius: var(--radius-md); background: var(--card); cursor: pointer; }
  .send { color: var(--on-accent); background: var(--accent); border-color: var(--accent); }
  .err { flex-basis: 100%; margin: 0; color: var(--st-failed-fg); }
</style>
