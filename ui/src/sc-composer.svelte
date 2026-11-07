<!-- SC-32: the thread's bottom bar (SUPERCONDUCTOR-27's 4-5). A field that grows;
     Enter sends when Claude is idle and queues while it works, Shift+Enter is a
     new line. Queued follow-ups stand above the field as cards you can edit,
     remove, or "Stop and send" (stop the turn; this goes first); they go in
     order, one per turn, when the turn ends — kept in this page only. While
     Claude works, a strip says for how long and offers Stop (Esc, by
     bus.keys). While an approval waits (`sc.pending`, said by sc-approval),
     (status `needs`; the request read with hosts.pending), typing offers "Deny
     and send this": hosts.answer's Deny carries a message Claude reads. On a phone it sticks to the bottom, above the safe area.

     Props: `status` is the thread's one status (sc-pill's: needs, working,
     waiting, failed, done, idle, offline); needs and working mean a turn is on.

     Mobile (menger v1's composer.rs, grow.rs): one line on a phone, Send inline,
     the field the whole row once its text wraps; it grows with the text
     (field-sizing, else its scrollHeight) up to ~45% of what is visible. Send keeps
     the keyboard up (a touch sends on touchend, a press never takes the focus).
     Up-arrow in the empty field brings back the last message sent. What you are
     writing is kept per thread (localStorage) until it is sent. `quote(text)` puts
     a quote of text in the field (sc-thread's long-press). The text stays small: a
     focused field is kept from zooming by index.html's viewport, not by 16px. -->
<script module>
  export const uses = ["hosts.send", "hosts.interrupt", "hosts.answer", "hosts.pending"];
  const clock = (ms) => { const s = Math.max(0, Math.floor(ms / 1000)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`; };
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Button from "./sc-button.svelte";
  import { keyboardUp, onKeyboard } from "./sc-keyboard.svelte";

  let { host, thread, status = "idle" } = $props();
  const b = bus();
  const busy = $derived(status === "working" || status === "needs");

  let text = $state("");
  let queued = $state([]);          // [{ id, text, editing }]
  let error = $state("");
  let sending = $state(false);
  let field = $state(null);
  let seq = 0;
  let kb = $state(keyboardUp());
  $effect(() => onKeyboard((up) => (kb = up)));

  // What you are writing, kept per thread until it is sent; the last message sent, for up-arrow.
  const store = (k) => `sc.${k}.${host}/${thread}`;
  const load = (k) => { try { return localStorage.getItem(store(k)) ?? ""; } catch { return ""; } };
  const save = (k, v) => { try { if (v) localStorage.setItem(store(k), v); else localStorage.removeItem(store(k)); } catch { /* private mode: kept in the page only */ } };
  let loadedFor = "";
  $effect(() => {
    const at = host + "/" + thread;
    if (at === loadedFor) return;
    loadedFor = at;
    text = load("draft");
  });
  $effect(() => { const v = text; if (loadedFor === host + "/" + thread) save("draft", v); });

  /** sc-thread's long-press Quote: the text quoted, then a line to answer on. */
  export function quote(said) {
    const q = String(said ?? "").trim().split("\n").map((l) => "> " + l).join("\n") + "\n\n";
    text = q + text;
    queueMicrotask(() => { if (!field) return; field.focus(); field.setSelectionRange(text.length, text.length); grow(); });
  }

  // The request waiting on this thread, if any (the oldest), while it needs you.
  let pending = $state(null);
  $effect(() => {
    if (status !== "needs") { pending = null; return; }
    let gone = false;
    b.call("hosts.pending", { host }).then((r) => {
      if (gone) return;
      const recs = (r.events ?? []).find((e) => e.type === "pending")?.requests ?? [];
      const rec = recs.find((x) => x.thread === thread && x.request?.kind?.kind !== "other");
      pending = rec ? { request: rec.request.id } : null;
    });
    return () => { gone = true; };
  });

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

  // Grow with the text (CSS field-sizing where there is one, else its scrollHeight,
  // again a frame later), up to the field's max-height; once the text wraps, the
  // field takes the whole row and Send goes under it.
  const sized = typeof CSS !== "undefined" && CSS.supports?.("field-sizing", "content");
  let wrapped = $state(false);
  let line = 0;                     // the field's height holding one line
  function grow() {
    if (!field) return;
    if (!sized) {
      field.style.height = "auto";
      const cap = parseFloat(getComputedStyle(field).maxHeight) || Infinity;
      field.style.height = Math.min(field.scrollHeight + 2, cap) + "px";
    }
    if (!text) { line = field.offsetHeight; wrapped = false; return; }
    if (!line) line = field.offsetHeight;
    wrapped = wrapped || field.offsetHeight > line + 4 || text.includes("\n");
  }
  $effect(() => { text; queueMicrotask(grow); requestAnimationFrame(grow); });

  async function submit(ev) {
    ev?.preventDefault();
    const prompt = text.trim();
    if (!prompt || sending) return;
    if (busy || queued.length) { queued.push({ id: ++seq, text: prompt, editing: false }); save("last", prompt); text = ""; return; }
    sending = true;
    text = "";
    save("last", prompt);
    if (!(await send(prompt))) text = prompt;
    else waiting = Date.now();
    sending = false;
  }
  function keydown(ev) {
    if (ev.isComposing || ev.keyCode === 229) return;
    if (ev.key === "Enter" && !ev.shiftKey) { ev.preventDefault(); submit(); }
    else if (ev.key === "ArrowUp" && !text) {
      const last = load("last");
      if (last) { ev.preventDefault(); text = last; queueMicrotask(() => field?.setSelectionRange(last.length, last.length)); }
    }
  }

  // Send keeps the keyboard up: a press never moves the focus off the field, and a
  // tap (lifted within 10px and 500ms) sends on touchend, before any blur.
  let touch = null;
  function touchstart(ev) { const t = ev.touches[0]; touch = t ? { x: t.clientX, y: t.clientY, at: Date.now() } : null; }
  function touchend(ev) {
    const t = ev.changedTouches[0], was = touch;
    touch = null;
    if (!was || !t || Math.hypot(t.clientX - was.x, t.clientY - was.y) > 10 || Date.now() - was.at > 500) return;
    if (ev.target.closest?.("button")?.disabled) return;
    ev.preventDefault();
    submit();
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
    if (!error) { text = ""; pending = null; }
  }
</script>

<div class="composer" data-surface="sc-composer" data-kb={kb ? "" : undefined}>
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
      <span>Or send this as your reason for denying Claude's request.</span>
      <Button kind="quiet" disabled={sending} onclick={denyWith}>Deny and send this</Button>
    </div>
  {/if}

  <form onsubmit={submit} class:wrap={wrapped}>
    <textarea bind:this={field} rows="1" bind:value={text} onkeydown={keydown} oninput={grow}
      enterkeyhint="send" autocomplete="off"
      aria-label={busy ? "Queue a follow-up" : "Message Claude"}
      placeholder={busy ? "Queue a follow-up" : "Reply to Claude"}></textarea>
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <span class="go" data-keep-kb onmousedown={(e) => e.preventDefault()} ontouchstart={touchstart} ontouchend={touchend}><Button kind="primary" type="submit" disabled={!text.trim() || sending}>{busy ? "Queue" : "Send"}</Button></span>
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
    padding: var(--s3) var(--s5) calc(var(--s3) + env(safe-area-inset-bottom));
    background: var(--panel); border-top: 1px solid var(--line);
  }
  .composer > * { max-width: 80ch; width: 100%; min-width: 0; }

  .queue { list-style: none; margin: 0; padding: 0; display: grid; gap: var(--s2); }
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

  form { display: flex; flex-wrap: wrap; gap: var(--s2); align-items: flex-end; }
  textarea {
    flex: 1 1 0; min-width: 0; min-height: 44px; max-height: 45dvh; padding: 10px var(--s3); resize: none;
    field-sizing: content; overflow-y: auto; overscroll-behavior: contain;
    font: inherit; line-height: var(--leading); color: var(--ink); background: var(--bg);
    border: 1px solid var(--line); border-radius: var(--radius-md);
  }
  /* The text wraps: the field is the whole row, Send under it at the right. */
  form.wrap textarea { flex-basis: 100%; }
  form.wrap .go { margin-left: auto; }
  .go { -webkit-tap-highlight-color: transparent; touch-action: manipulation; }
  textarea:focus-visible { outline: 2px solid var(--focus); outline-offset: -1px; border-color: var(--focus); }
  .go :global(button) { min-height: 44px; min-width: 76px; }
  .keys { margin: 0; font-size: 12px; color: var(--muted); }
  .err { margin: 0; color: var(--st-failed-fg); font-size: 13px; }
  .link { padding: 0; font: inherit; color: var(--accent); background: none; border: none; text-decoration: underline; cursor: pointer; }

  @keyframes pulse { from { opacity: 1; } to { opacity: .35; } }
  @media (prefers-reduced-motion: reduce) { .dot { animation: none; } }
  @media (max-width: 719px) {
    .composer { padding-left: var(--s4); padding-right: var(--s4); }
    .composer { padding-top: var(--s2); }
    .go :global(button) { min-width: 64px; }
    /* The keyboard up: the field is held within the visible area, and the composer sits right on the keyboard. */
    .composer[data-kb] { padding-bottom: var(--s2); }
    [data-kb] textarea { max-height: calc(var(--kb-h) * .45); }
    .keys, kbd { display: none; }
    .qa :global(button) { min-height: 44px; flex: 1; padding: 0 var(--s2); white-space: nowrap; }
    .stop :global(button) { min-height: 44px; }
  }
</style>
