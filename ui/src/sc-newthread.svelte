<!-- SUPERCONDUCTOR-34: `#/h/:host/new`, starting a thread composer-first (after
     T3 and Claude desktop): the prompt first and large; below it the folder, from
     the folders this machine's threads already use or any other path; the
     machine (any of yours, online shown); the model, the one other option
     hosts.start takes. Start, or Cmd/Ctrl+Enter, lands on the new thread. The
     folder is remembered per machine in this browser. Main content only. -->
<script module>
  export const uses = ["hosts.list", "hosts.threads", "hosts.start"];
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Button from "./sc-button.svelte";
  import Field from "./sc-field.svelte";

  let { host } = $props();
  const b = bus();
  const MODELS = [["", "Default"], ["opus", "Opus"], ["sonnet", "Sonnet"], ["haiku", "Haiku"]];

  let hosts = $state([]);
  let info = $state(undefined);
  let recent = $state([]);
  let prompt = $state("");
  let folder = $state("");
  let other = $state(false);
  let model = $state("");
  let starting = $state(false);
  let said = $state("");
  let field;

  const event = (r, type) => (r.events ?? []).find((e) => e.type === type);
  const failure = (r) => event(r, "failed")?.message ?? (r.ok ? "" : (r.message ?? ""));
  const key = $derived("superconductor.cwd." + host);
  const remembered = (k) => { try { return localStorage.getItem(k) || ""; } catch { return ""; } };
  const base = (p) => (p ?? "").replace(/\/+$/, "").split("/").pop() || p;
  const mac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

  async function load() {
    const r = await b.call("hosts.list", {});
    hosts = event(r, "hosts")?.hosts ?? [];
    info = hosts.find((h) => h.name === host) ?? null;
    if (!info?.online) return;
    const t = await b.call("hosts.threads", { host });
    const rows = (event(t, "threads")?.threads ?? []).slice().sort((x, y) => (y.last_seq ?? 0) - (x.last_seq ?? 0));
    const seen = [];
    const last = remembered(key);
    if (last) seen.push(last);
    for (const row of rows) if (row.cwd && !seen.includes(row.cwd)) seen.push(row.cwd);
    recent = seen.slice(0, 6);
    if (!folder && !other) {
      if (recent.length) folder = recent[0];
      else other = true;
    }
  }
  $effect(() => {
    host;
    info = undefined; recent = []; folder = ""; other = false; said = "";
    load();
  });
  $effect(() => { if (info?.online && field) field.focus(); });

  const cwd = $derived(folder.trim());
  const ready = $derived(!!info?.online && !!cwd && !!prompt.trim() && !starting);

  async function start(ev) {
    ev?.preventDefault?.();
    if (!ready) {
      if (info?.online && !prompt.trim()) said = "Write what Claude should do first.";
      else if (info?.online && !cwd) said = "Pick a folder for Claude to work in.";
      return;
    }
    starting = true; said = "";
    try { localStorage.setItem(key, cwd); } catch { /* a private window */ }
    const params = { host, cwd, prompt };
    if (model) params.model = model;
    const r = await b.call("hosts.start", params);
    starting = false;
    const started = event(r, "started");
    if (started) {
      prompt = "";
      b.navigate(`#/h/${encodeURIComponent(host)}/t/${encodeURIComponent(started.thread)}`);
    } else said = failure(r) || `${host} didn't start the thread. Check it's still online and try again.`;
  }
  b.keys({
    "ctrl+enter": { to: () => start(), label: "Start the thread", in_inputs: true },
    "meta+enter": { to: () => start(), label: "Start the thread", in_inputs: true },
  });
  function pick(path) { folder = path; other = false; }
  function pickOther() { other = true; folder = ""; }
</script>

<div class="sc-page">
  <form onsubmit={start}>
    <header class="head">
      <h1>New thread</h1>
      {#if hosts.length > 1}
        <label class="on">on
          <select value={host} onchange={(e) => b.navigate(`#/h/${encodeURIComponent(e.currentTarget.value)}/new`)} aria-label="Machine">
            {#each hosts as h (h.name)}<option value={h.name}>{h.name}{h.online ? "" : " (offline)"}</option>{/each}
          </select>
        </label>
      {:else}
        <span class="on">on <strong>{host}</strong></span>
      {/if}
    </header>

    {#if info === null}
      <p class="error">{host} isn't one of your machines. <a href="#/">See your machines</a></p>
    {:else if info && !info.online}
      <p class="offline" role="status">{host} is offline, so nothing can start there right now. Run <code>plexus-agents host</code> on it and this page is ready when it reconnects.</p>
    {/if}

    <textarea
      bind:this={field}
      class="prompt"
      value={prompt}
      oninput={(e) => (prompt = e.currentTarget.value)}
      placeholder="What should Claude do?"
      aria-label="What should Claude do?"
      rows="7"
      disabled={info !== undefined && !info?.online}
    ></textarea>

    <fieldset class="folders" disabled={!info?.online}>
      <legend>Folder on {host}</legend>
      <div class="chips" role="radiogroup" aria-label="Folder">
        {#each recent as path (path)}
          <button type="button" class="chip" role="radio" aria-checked={!other && folder === path} onclick={() => pick(path)} title={path}>
            <span class="chip-name">{base(path)}</span><span class="chip-path">{path}</span>
          </button>
        {/each}
        <button type="button" class="chip" role="radio" aria-checked={other} onclick={pickOther}>
          <span class="chip-name">Other folder…</span>
        </button>
      </div>
      {#if other}
        <Field hint={recent.length ? "" : `Folders you use here show up as choices next time.`}>
          <input class="path" value={folder} oninput={(e) => (folder = e.currentTarget.value)} placeholder="/Users/you/project" aria-label="Folder path" spellcheck="false" autocapitalize="off" autocomplete="off"/>
        </Field>
      {/if}
    </fieldset>

    <div class="foot">
      <label class="model">Model
        <select value={model} onchange={(e) => (model = e.currentTarget.value)} disabled={!info?.online}>
          {#each MODELS as [v, name] (v)}<option value={v}>{name}</option>{/each}
        </select>
      </label>
      <span class="spacer"></span>
      <span class="hint" aria-hidden="true">{mac ? "⌘ Enter" : "Ctrl Enter"}</span>
      <Button kind="primary" type="submit" disabled={!info?.online || starting}>{starting ? "Starting…" : "Start thread"}</Button>
    </div>
    {#if said}<p class="error" role="alert">{said}</p>{/if}
  </form>
</div>

<style>
  .sc-page { max-width: 760px; padding: var(--s6) var(--s5); }
  form { display: grid; gap: var(--s4); }
  .head { display: flex; align-items: baseline; flex-wrap: wrap; gap: var(--s3); }
  h1 { margin: 0; font-size: 22px; font-weight: 600; letter-spacing: -0.01em; }
  .on { color: var(--muted); display: inline-flex; align-items: baseline; gap: var(--s2); }
  .on strong { color: var(--ink); font-weight: 500; }
  p { margin: 0; }
  .error { color: var(--st-failed-fg); }
  .offline { padding: var(--s3) var(--s4); color: var(--st-offline-fg); background: var(--st-offline-bg); border-radius: var(--radius-sm); }
  code { font-family: var(--font-mono); font-size: 13px; }

  .prompt { width: 100%; min-height: 180px; padding: var(--s4); font-size: 16px; line-height: var(--leading); color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-md); resize: vertical; }
  .prompt:focus { outline: none; border-color: var(--focus); box-shadow: 0 0 0 1px var(--focus); }
  .prompt:disabled { background: var(--hover); }

  fieldset { margin: 0; padding: 0; border: 0; min-width: 0; display: grid; gap: var(--s3); }
  legend { padding: 0; margin-bottom: var(--s2); font-weight: 500; }
  .chips { display: flex; flex-wrap: wrap; gap: var(--s2); }
  .chip { display: inline-flex; flex-direction: column; align-items: flex-start; justify-content: center; min-height: 44px; max-width: 100%; padding: var(--s1) var(--s3); text-align: left; color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); cursor: pointer; }
  .chip:hover { background: var(--hover); }
  .chip[aria-checked=true] { background: var(--sel); border-color: var(--accent); box-shadow: inset 0 0 0 1px var(--accent); }
  .chip:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
  .chip:disabled { color: var(--disabled); cursor: default; }
  .chip-name { font-weight: 500; }
  .chip-path { max-width: 32ch; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font: 12px var(--font-mono); color: var(--muted); }
  .path { width: 100%; min-height: 44px; padding: 0 var(--s3); font: 15px var(--font-mono); color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); }
  .path:focus { outline: none; border-color: var(--focus); box-shadow: 0 0 0 1px var(--focus); }

  .foot { display: flex; align-items: center; flex-wrap: wrap; gap: var(--s3); }
  .model { display: inline-flex; align-items: center; gap: var(--s2); color: var(--muted); }
  select { min-height: 44px; padding: 0 var(--s2); color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); }
  select:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
  .spacer { flex: 1; }
  .hint { color: var(--muted); font-size: 13px; }

  @media (max-width: 719px) {
    .sc-page { padding: var(--s4); }
    .prompt { min-height: 140px; }
    .chips { flex-direction: column; }
    .chip { width: 100%; }
    .chip-path { max-width: 100%; }
    .hint { display: none; }
    .foot :global(.sc-button) { width: 100%; }
  }
</style>
