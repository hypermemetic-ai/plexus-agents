<!-- SUPERCONDUCTOR-34: `#/h/:host/new`, starting a thread composer-first (after
     T3 and Claude desktop): the prompt first and large; below it the folder, the
     permission mode, the model. Start, or Cmd/Ctrl+Enter, lands on the new thread.
     Main content only.

     superconductor-ui/modes: the folder is one field, "Folder path", that both
     filters the host's folders as you type and takes any path typed in full. The
     list under it (`hosts.folders`) is the folders recent threads used first, then
     the projects under the host's roots, a git checkout marked; "Other folder…"
     clears the field for a path of your own. The last folder is remembered per
     machine in this browser. The permission mode (sc-modes) is offered when the
     host says what it allows (`hosts.capabilities`); bypass only where the host
     was started with --allow-bypass. A host from before modes gets neither, and
     its folders come from its threads as before. -->
<script module>
  export const uses = ["hosts.list", "hosts.threads", "hosts.start", "hosts.folders", "hosts.capabilities"];
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Button from "./sc-button.svelte";
  import Field from "./sc-field.svelte";
  import Modes from "./sc-modes.svelte";

  let { host } = $props();
  const b = bus();
  const MODELS = [["", "Default"], ["opus", "Opus"], ["sonnet", "Sonnet"], ["haiku", "Haiku"]];

  let hosts = $state([]);
  let info = $state(undefined);
  let folders = $state([]);        // [{ path, name, recent, git }]
  let roots = $state([]);
  let caps = $state(undefined);    // the host's capabilities; null: a host from before modes
  let prompt = $state("");
  let folder = $state("");
  let mode = $state("default");
  let model = $state("");
  let starting = $state(false);
  let said = $state("");
  let field;
  let pathField = $state(null);

  const event = (r, type) => (r.events ?? []).find((e) => e.type === type);
  const failure = (r) => event(r, "failed")?.message ?? (r.ok ? "" : (r.message ?? ""));
  const key = $derived("superconductor.cwd." + host);
  const remembered = (k) => { try { return localStorage.getItem(k) || ""; } catch { return ""; } };
  const base = (p) => (p ?? "").replace(/\/+$/, "").split("/").pop() || p;
  const home = (p) => (p ?? "").replace(/^\/(Users|home)\/[^/]+/, "~");
  const mac = typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);

  async function load() {
    const r = await b.call("hosts.list", {});
    hosts = event(r, "hosts")?.hosts ?? [];
    info = hosts.find((h) => h.name === host) ?? null;
    if (!info?.online) return;
    const [t, f, c] = await Promise.all([
      b.call("hosts.threads", { host }),
      b.call("hosts.folders", { host }),
      b.call("hosts.capabilities", { host }),
    ]);
    caps = event(c, "capabilities") ?? null;
    const listed = event(f, "folders");
    roots = listed?.roots ?? caps?.roots ?? [];
    // Recent first: this browser's last, the threads' own, then the host's.
    const rows = (event(t, "threads")?.threads ?? []).slice().sort((x, y) => (y.last_seq ?? 0) - (x.last_seq ?? 0));
    const out = [];
    const add = (path, extra = {}) => {
      if (!path) return;
      const had = out.find((x) => x.path === path);
      if (had) { Object.assign(had, { git: had.git || !!extra.git }); return; }
      out.push({ path, name: base(path), recent: false, git: false, ...extra });
    };
    const last = remembered(key);
    if (last) add(last, { recent: true });
    for (const row of rows) add(row.cwd, { recent: true });
    for (const x of listed?.folders ?? []) add(x.path, { name: x.name || base(x.path), recent: !!x.recent, git: !!x.git });
    out.sort((x, y) => Number(y.recent) - Number(x.recent));
    folders = out;
    if (!folder && out.length && out[0].recent) folder = out[0].path;
  }
  function fresh() {
    info = undefined; folders = []; roots = []; caps = undefined; folder = ""; mode = "default"; said = "";
    load();
  }
  $effect(() => {
    host;
    fresh();
    // The engine may keep this page mounted between visits: coming back to it
    // re-reads the folders.
    const here = () => { if (location.hash === `#/h/${encodeURIComponent(host)}/new`) fresh(); };
    window.addEventListener("hashchange", here);
    return () => window.removeEventListener("hashchange", here);
  });
  $effect(() => { if (info?.online && field) field.focus(); });

  const cwd = $derived(folder.trim());
  const ready = $derived(!!info?.online && !!cwd && !!prompt.trim() && !starting);
  // Typing filters; a folder picked from the list shows them all again.
  const query = $derived(folders.some((x) => x.path === cwd) ? "" : cwd.toLowerCase());
  const shown = $derived(query ? folders.filter((x) => x.path.toLowerCase().includes(query) || x.name.toLowerCase().includes(query)) : folders);
  const recentShown = $derived(shown.filter((x) => x.recent));
  const projectsShown = $derived(shown.filter((x) => !x.recent));
  const where = $derived(roots.length === 1 ? `In ${home(roots[0])}` : "In your project folders");
  const hasModes = $derived(!!caps?.modes?.length);
  // Under a root, a folder is said from the root (the group head names it); a
  // folder only one level down needs nothing past its name.
  function from(x) {
    const r = roots.find((r) => x.path === r || x.path.startsWith(r.replace(/\/+$/, "") + "/"));
    if (!r) return home(x.path);
    const rel = x.path.slice(r.replace(/\/+$/, "").length + 1);
    return rel === x.name ? "" : rel;
  }
  const typedOwn = $derived(!!cwd && !folders.some((x) => x.path === cwd));

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
    if (hasModes && mode !== "default") params.mode = mode;
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
  function pick(path) { folder = path; }
  function pickOther() { folder = ""; pathField?.focus(); }
</script>

{#snippet row(x)}
  <button type="button" class="row" role="radio" aria-checked={cwd === x.path} onclick={() => pick(x.path)} title={x.path}>
    <span class="name">{x.name}</span>
    {#if x.git}<span class="git" title="A git checkout"><svg viewBox="0 0 16 16" width="12" height="12" aria-hidden="true"><circle cx="4" cy="3.5" r="1.75"/><circle cx="4" cy="12.5" r="1.75"/><circle cx="12" cy="5.5" r="1.75"/><path d="M4 5.25v5.5M12 7.25c0 2.5-3 2.5-6.5 4"/></svg><span class="sr">git</span></span>{/if}
    {#if from(x)}<span class="where">{from(x)}</span>{/if}
  </button>
{/snippet}

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
      <Field hint={typedOwn ? `Claude starts in ${cwd} as typed.` : folders.length ? "Type to filter, or type any folder path." : `Type the full path of a folder on ${host}.`}>
        <input bind:this={pathField} class="path" value={folder} oninput={(e) => (folder = e.currentTarget.value)} placeholder="/Users/you/project" aria-label="Folder path" spellcheck="false" autocapitalize="off" autocomplete="off"/>
      </Field>
      {#if folders.length}
        <div class="list" role="radiogroup" aria-label="Folder">
          {#if recentShown.length}
            <p class="group">Recently used</p>
            {#each recentShown as x (x.path)}{@render row(x)}{/each}
          {/if}
          {#if projectsShown.length}
            <p class="group">{where}</p>
            {#each projectsShown as x (x.path)}{@render row(x)}{/each}
          {/if}
          {#if !shown.length}
            <p class="none">No folder here matches “{cwd}”. Type its full path to use it anyway.</p>
          {/if}
          <button type="button" class="row other" role="radio" aria-checked={false} onclick={pickOther}>
            <span class="name">Other folder…</span>
          </button>
        </div>
      {/if}
    </fieldset>

    {#if hasModes}
      <fieldset class="modes" disabled={!info?.online}>
        <legend>Permissions</legend>
        <Modes value={mode} onpick={(m) => (mode = m)} bypass={caps?.bypass ?? null} {host} label="Permissions"/>
      </fieldset>
    {/if}

    <div class="foot">
      <label class="model">Model
        <select value={model} onchange={(e) => (model = e.currentTarget.value)} disabled={!info?.online}>
          {#each MODELS as [v, name] (v)}<option value={v}>{name}</option>{/each}
        </select>
      </label>
      <span class="spacer"></span>
      <span class="hint" aria-hidden="true">{mac ? "⌘ Enter" : "Ctrl Enter"}</span>
      <Button kind={mode === "bypassPermissions" ? "danger" : "primary"} type="submit" disabled={!info?.online || starting}>{starting ? "Starting…" : mode === "bypassPermissions" ? "Start without permissions" : "Start thread"}</Button>
    </div>
    {#if said}<p class="error" role="alert">{said}</p>{/if}
  </form>
</div>

<style>
  .sc-page { max-width: 760px; padding: var(--s6) var(--s5); }
  form { display: grid; grid-template-columns: minmax(0, 1fr); gap: var(--s5); }
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

  fieldset { margin: 0; padding: 0; border: 0; min-width: 0; display: grid; grid-template-columns: minmax(0, 1fr); gap: var(--s2); }
  legend { padding: 0; margin-bottom: var(--s2); font-weight: 500; }
  .path { width: 100%; min-height: 44px; padding: 0 var(--s3); font: 15px var(--font-mono); color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); }
  .path:focus { outline: none; border-color: var(--focus); box-shadow: 0 0 0 1px var(--focus); }

  /* The folders: one list, rows on hairlines, scrolled inside past ~8. */
  .list { display: grid; max-height: 340px; overflow-y: auto; overscroll-behavior: contain; border: 1px solid var(--line); border-radius: var(--radius-sm); background: var(--card); }
  .group { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; position: sticky; top: 0; z-index: 1; padding: var(--s2) var(--s3) var(--s1); font-size: var(--text-small); color: var(--muted); background: var(--card); }
  .none { padding: var(--s3); color: var(--muted); font-size: var(--text-note); }
  .row {
    display: flex; align-items: baseline; gap: var(--s2); min-height: 40px; min-width: 0; padding: var(--s2) var(--s3);
    text-align: left; font: inherit; color: var(--ink); background: none; border: 0; border-top: 1px solid var(--line); cursor: pointer;
  }
  .group + .row { border-top: 0; }
  .row:hover { background: var(--hover); }
  .row:focus-visible { outline: 2px solid var(--focus); outline-offset: -2px; }
  .row[aria-checked=true] { background: var(--sel); box-shadow: inset 3px 0 0 var(--accent); }
  .row:disabled { color: var(--disabled); cursor: default; }
  .row .name { flex: none; max-width: 50%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 500; }
  .row .where { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font: 12px var(--font-mono); color: var(--muted); }
  .other .name { color: var(--accent); }
  .git { flex: none; align-self: center; display: inline-flex; color: var(--muted); }
  .git svg { fill: none; stroke: currentColor; stroke-width: 1.4; }
  .sr { position: absolute; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; }

  .foot { display: flex; align-items: center; flex-wrap: wrap; gap: var(--s3); }
  .model { display: inline-flex; align-items: center; gap: var(--s2); color: var(--muted); }
  select { min-height: 44px; padding: 0 var(--s2); color: var(--ink); background: var(--card); border: 1px solid var(--line); border-radius: var(--radius-sm); }
  select:focus-visible { outline: 2px solid var(--focus); outline-offset: 1px; }
  .spacer { flex: 1; }
  .hint { color: var(--muted); font-size: 13px; }

  @media (max-width: 719px) {
    .sc-page { padding: var(--s4); }
    form { gap: var(--s4); }
    .prompt { min-height: 140px; }
    .list { max-height: 300px; }
    .row { min-height: var(--tap); flex-wrap: wrap; align-items: center; row-gap: 0; }
    .row .where:empty { display: none; }
    .row .name { max-width: 100%; }
    .row .where { flex-basis: 100%; }
    .hint { display: none; }
    .foot :global(.sc-button) { width: 100%; }
  }
</style>
