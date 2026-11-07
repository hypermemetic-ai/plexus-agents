<!-- SC-32: the thread's bottom bar (SUPERCONDUCTOR-27's 4-5). A field that grows;
     Enter sends when Claude is idle and queues while it works, Shift+Enter is a
     new line. Queued follow-ups stand above the field as cards you can edit,
     remove, or "Stop and send" (stop the turn; this goes first); they go in
     order, one per turn, when the turn ends — kept in this page only. While
     Claude works, a strip says for how long and offers Stop (Esc, by
     bus.keys). While an approval waits (`sc.pending`, said by sc-approval),
     typing offers "Deny and send this": hosts.answer's Deny carries a message
     Claude reads. On a phone it sticks to the bottom, above the safe area.

     Props: `status` is the thread's one status (sc-pill's: needs, working,
     waiting, failed, done, idle, offline); needs and working mean a turn is on. -->
<script module>
  export const uses = ["hosts.send", "hosts.interrupt", "hosts.answer"];
  const clock = (ms) => { const s = Math.max(0, Math.floor(ms / 1000)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`; };
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Button from "./sc-button.svelte";

  let { host, thread, status = "idle" } = $props();
  const b = bus();
  const busy = $derived(status === "working" || status === "needs");

  let text = $state("");
  let queued = $state([]);          // [{ id, text, editing }]
  let error = $state("");
  let sending = $state(false);
  let field = $state(null);
  let seq = 0;

  // The pending approval on this thread, if any.
  let pending = $state(null);
  $effect(() => b.on("sc.pending", (ev) => {
    pending = ev && ev.host === host && ev.thread === thread ? ev : null;
  }, { last: true }));

  // How long this turn has run (counted from when this page saw it start).
  let since = $state(0);
  let now = $state(Date.now());
  $effect(() => {
    if (!busy) { since = 0; return; }
    since = Date.now(); now = since;
    const t = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(t);
  });

  function failure(r, doing) {
    const f = (r.events ?? []).find((e) => e.type === "failed");
    if (r.ok && !f) return "";
    if (r.message === "disconnected") return `Couldn't ${doing}: the connection dropped. Check you're online and try again.`;
    return `Couldn't ${doing}: ${f?.message ?? r.message ?? "no reply"}.`;
  }
  async function send(prompt) {
    const r = await b.call("hosts.send", { host, thread, prompt });
    error = failure(r, "send your message");
    return !error;
  }

  // Grow with the text, up to a third of the screen.
  function grow() {
    if (!field) return;
    field.style.height = "auto";
    field.style.height = Math.min(field.scrollHeight, Math.max(120, window.innerHeight / 3)) + "px";
  }
  $effect(() => { text; queueMicrotask(grow); });

  async function submit(ev) {
    ev?.preventDefault();
    const prompt = text.trim();
    if (!prompt || sending) return;
    if (busy || queued.length) { queued.push({ id: ++seq, text: prompt, editing: false }); text = ""; return; }
    sending = true;
    text = "";
    if (!(await send(prompt))) text = prompt;
    else waiting = Date.now();
    sending = false;
  }
  function keydown(ev) {
    if (ev.key === "Enter" && !ev.shiftKey && !ev.isComposing) { ev.preventDefault(); submit(); }
  }

  // The queue goes, in order, once the turn is over: one, then wait for its turn.
  let waiting = 0;                  // when the last send went, until its turn shows as busy
  let held = $state(false);         // a queued send failed: wait for the person
  $effect(() => { if (busy) waiting = 0; });
  $effect(() => {
    void now;
    if (busy || sending || held || !queued.length || queued[0].editing) return;
    if (waiting && Date.now() - waiting < 15000) { const t = setTimeout(() => (now = Date.now()), 1000); return () => clearTimeout(t); }
    flush();
  });
  async function flush() {
    const next = queued[0];
    sending = true;
    const ok = await send(next.text);
    sending = false;
    if (ok) { queued.splice(queued.indexOf(next), 1); waiting = Date.now(); }
    else held = true;
  }
  function remove(q) { queued.splice(queued.indexOf(q), 1); }
  async function sendNow(q) {
    queued.splice(queued.indexOf(q), 1);
    queued.unshift({ ...q, editing: false });
    held = false;
    if (busy) await stop();
  }
  function retry() { held = false; error = ""; }

  async function stop() {
    const r = await b.call("hosts.interrupt", { host, thread });
    error = failure(r, "stop Claude");
  }
  $effect(() => {
    if (!busy) return;
    return b.keys({ escape: { to: (ev) => {
      // Esc in a queued card's field closes the edit, not the turn.
      const q = queued.find((x) => ev?.target?.name === "queued-" + x.id);
      if (q) q.editing = false; else stop();
    }, label: "stop Claude", in_inputs: true } });
  });

  async function denyWith() {
    const message = text.trim();
    if (!message || !pending) return;
    const request = pending.request;
    sending = true;
    const r = await b.call("hosts.answer", { host, thread, request, answer: { answer: "deny", message, stop_turn: false } });
    sending = false;
    error = failure(r, "deny with your note");
    if (!error) { text = ""; b.emit("sc.answered", { request, outcome: "denied", note: message }); }
  }
</script>

<div class="composer" data-surface="sc-composer">
  {#if queued.length}
    <ol class="queue" aria-label="Queued follow-ups">
      {#each queued as q, n (q.id)}
        <li class="queued">
          <span class="when">{n === 0 ? (busy ? "Sends when Claude finishes" : "Sending next") : `Then, ${n === 1 ? "second" : n + 1 + "th"}`}</span>
          {#if q.editing}
            <textarea class="edit" rows="2" name={"queued-" + q.id} aria-label="Edit this follow-up" bind:value={q.text}
              onkeydown={(e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); q.editing = false; } if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); q.editing = false; } }}></textarea>
          {:else}
            <p class="qt">{q.text}</p>
          {/if}
          <div class="qa">
            {#if q.editing}
              <Button kind="quiet" onclick={() => (q.editing = false)}>Done</Button>
            {:else}
              <Button kind="quiet" onclick={() => (q.editing = true)}>Edit</Button>
            {/if}
            <Button kind="quiet" onclick={() => remove(q)}>Remove</Button>
            {#if busy}<Button kind="quiet" onclick={() => sendNow(q)}>Stop and send</Button>{/if}
          </div>
        </li>
      {/each}
    </ol>
  {/if}

  {#if busy}
    <div class="strip" data-status={status}>
      <span class="dot" aria-hidden="true"></span>
      <span class="what">{status === "needs" ? "Waiting for your answer" : "Working"}</span>
      <span class="time">{since ? clock(now - since) : ""}</span>
      <span class="stop"><Button kind="danger" onclick={stop}>Stop<kbd>esc</kbd></Button></span>
    </div>
  {/if}

  {#if pending && text.trim()}
    <div class="deny">
      <span>Claude is waiting on “{pending.what}”.</span>
      <Button kind="quiet" disabled={sending} onclick={denyWith}>Deny and send this</Button>
    </div>
  {/if}

  <form onsubmit={submit}>
    <textarea bind:this={field} rows="1" bind:value={text} onkeydown={keydown} oninput={grow}
      aria-label={busy ? "Queue a follow-up" : "Message Claude"}
      placeholder={busy ? "Queue a follow-up for when Claude finishes" : "Reply to Claude"}></textarea>
    <span class="go"><Button kind="primary" type="submit" disabled={!text.trim() || sending}>{busy ? "Queue" : "Send"}</Button></span>
  </form>
  <p class="keys">Enter {busy ? "queues" : "sends"}, Shift+Enter for a new line</p>

  {#if error}
    <p class="err" role="alert">{error}{#if held} The queue is paused. <button class="link" type="button" onclick={retry}>Try again</button>{/if}</p>
  {/if}
</div>

<style>
  .composer {
    position: sticky; bottom: 0; z-index: 2;
    display: grid; gap: var(--s2); min-width: 0;
    padding: var(--s3) var(--s5) max(var(--s3), env(safe-area-inset-bottom));
    background: var(--panel); border-top: 1px solid var(--line);
  }
  .composer > * { max-width: 80ch; width: 100%; margin: 0 auto; }

  .queue { list-style: none; margin: 0 auto; padding: 0; display: grid; gap: var(--s2); }
  .queued { display: grid; gap: var(--s1); padding: var(--s2) var(--s3); background: var(--card); border: 1px dashed var(--line); border-radius: var(--radius-md); min-width: 0; }
  .when { font-size: 12px; color: var(--muted); }
  .qt { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; max-height: 6lh; overflow: auto; }
  .qa { display: flex; flex-wrap: wrap; gap: var(--s1); }
  .qa :global(button) { min-height: 32px; padding: 0 var(--s3); font-size: 13px; }
  .edit { width: 100%; }

  .strip { display: flex; align-items: center; gap: var(--s2); color: var(--st-working-fg); }
  .strip[data-status=needs] { color: var(--st-needs-fg); }
  .dot { width: 8px; height: 8px; border-radius: var(--radius-pill); background: currentColor; animation: pulse 1.2s ease-in-out infinite alternate; }
  .what { font-weight: 500; }
  .time { font-variant-numeric: tabular-nums; color: var(--muted); }
  .stop { margin-left: auto; }
  .stop :global(button) { display: inline-flex; align-items: center; gap: var(--s2); min-height: 36px; }
  kbd { font: 500 11px/16px var(--font-mono); padding: 0 var(--s1); border: 1px solid currentColor; border-radius: var(--radius-xs); opacity: .65; }

  .deny { display: flex; align-items: center; flex-wrap: wrap; gap: var(--s2) var(--s3); padding: var(--s2) var(--s3); color: var(--st-needs-fg); background: var(--st-needs-bg); border-radius: var(--radius-md); }
  .deny span { flex: 1; min-width: 12ch; }
  .deny :global(button) { min-height: 36px; }

  form { display: flex; gap: var(--s2); align-items: flex-end; }
  textarea {
    flex: 1; min-width: 0; min-height: 44px; padding: 10px var(--s3); resize: none;
    font: inherit; line-height: var(--leading); color: var(--ink); background: var(--bg);
    border: 1px solid var(--line); border-radius: var(--radius-md);
  }
  textarea:focus-visible { outline: 2px solid var(--focus); outline-offset: -1px; border-color: var(--focus); }
  .go :global(button) { min-height: 44px; min-width: 76px; }
  .keys { margin: 0 auto; font-size: 12px; color: var(--muted); }
  .err { margin: 0 auto; color: var(--st-failed-fg); font-size: 13px; }
  .link { padding: 0; font: inherit; color: var(--accent); background: none; border: none; text-decoration: underline; cursor: pointer; }

  @keyframes pulse { from { opacity: 1; } to { opacity: .35; } }
  @media (prefers-reduced-motion: reduce) { .dot { animation: none; } }
  @media (max-width: 719px) {
    .composer { padding-left: var(--s4); padding-right: var(--s4); }
    textarea { font-size: 16px; }
    .keys, kbd { display: none; }
    .qa :global(button) { min-height: 44px; flex: 1; }
    .stop :global(button) { min-height: 44px; }
  }
</style>
