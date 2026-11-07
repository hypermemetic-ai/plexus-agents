<!-- The people page (`#/people`), test deployments only: an admin sees the app as
     someone else sees it. Everyone this service knows (people.known_users), each
     with Act as; and a test user made on the spot — a name and the roles the
     service offers — then acted as (people.create_user). Acting swaps the browser's
     session for that person's (plexus-login keeps the admin behind it), so the
     page reloads into the app as them; sc-top's band brings the admin back.
     Main content only. -->
<script module>
  export const uses = ["people.whoami", "people.known_users", "people.act_as", "people.create_user"];
</script>

<script>
  import { bus } from "plexus-ui/host";
  import Button from "./sc-button.svelte";
  import Field from "./sc-field.svelte";
  import Empty from "./sc-empty.svelte";

  const b = bus();
  let state = $state("loading"); // loading | off | ready
  let roles = $state([]);
  let users = $state([]);
  let error = $state("");
  let name = $state("");
  let picked = $state([]);
  let busy = $state("");

  const refused = (r) => (r.events ?? []).find((e) => e.type === "refused")?.message ?? r.message ?? "The server gave no answer.";

  async function load() {
    const who = await b.call("people.whoami", {}).catch(() => ({}));
    const me = (who.events ?? []).find((e) => e.type === "whoami");
    if (!me || !me.can_act) { state = "off"; return; }
    const r = await b.call("people.known_users", {});
    const ev = r.events ?? [];
    roles = ev.filter((e) => e.type === "role");
    users = ev.filter((e) => e.type === "user");
    if (!ev.length) error = refused(r);
    if (!picked.length && roles.length) picked = [roles[0].id];
    state = "ready";
  }
  $effect(() => { load(); });

  function into() { location.assign("/#/"); location.reload(); }

  async function actAs(u) {
    busy = u.principal; error = "";
    const r = await b.call("people.act_as", { principal: u.principal });
    if ((r.events ?? []).some((e) => e.type === "became")) return into();
    busy = ""; error = refused(r);
  }
  async function create(ev) {
    ev.preventDefault();
    if (!name.trim()) return;
    busy = "new"; error = "";
    const r = await b.call("people.create_user", { name: name.trim(), roles: picked });
    if ((r.events ?? []).some((e) => e.type === "became")) return into();
    busy = ""; error = refused(r);
  }
  function toggle(id) { picked = picked.includes(id) ? picked.filter((p) => p !== id) : [...picked, id]; }
  const roleText = (u) => (u.roles.filter((r) => !r.startsWith("owner@")).join(", ") || "No role yet");
</script>

<div class="sc-page" data-surface="sc-people">
  {#if state === "loading"}
    <p class="quiet">Loading…</p>
  {:else if state === "off"}
    <Empty title="Acting as someone is for admins on a test deployment" body="Sign in as an admin on test to see the app as another person sees it.">
      <p><a class="link" href="#/">Back to your threads</a></p>
    </Empty>
  {:else}
    <h1>Act as someone</h1>
    <p class="lede">See this app exactly as another person sees it. A band at the top stays while you act; use it to return to yourself.</p>

    <section class="block" aria-labelledby="new-h">
      <h2 id="new-h">Create a test user</h2>
      <form class="create" onsubmit={create}>
        <Field label="Name" hint="Letters, digits, - _ and . — becomes oidc:test/<name>">
          <input value={name} oninput={(e) => (name = e.currentTarget.value)} autocomplete="off" autocapitalize="none" spellcheck="false" placeholder="carol" aria-label="Name"/>
        </Field>
        <fieldset class="roles">
          <legend>Roles</legend>
          {#each roles as r (r.id)}
            <label class="role"><input type="checkbox" checked={picked.includes(r.id)} onchange={() => toggle(r.id)}/><span>{r.label}</span><code>{r.id}</code></label>
          {:else}
            <p class="quiet">This service offers no role to give: the user signs in with what everyone has.</p>
          {/each}
        </fieldset>
        <div><Button kind="primary" type="submit" disabled={!!busy || !name.trim()}>{busy === "new" ? "Creating…" : "Create and act as"}</Button></div>
      </form>
    </section>

    {#if error}<p class="error" role="alert">{error}</p>{/if}

    <section class="block" aria-labelledby="known-h">
      <h2 id="known-h">People this service knows</h2>
      <ul class="people">
        {#each users as u (u.principal)}
          <li class="person" data-you={u.you ? "" : undefined}>
            <span class="avatar" aria-hidden="true">{(u.name || u.principal.split(/[:/]/).pop() || "?").charAt(0).toUpperCase()}</span>
            <span class="who">
              <span class="pname">{u.name || u.principal.split("/").pop()}{#if u.you}<span class="tag">You</span>{/if}{#if u.test}<span class="tag">Test user</span>{/if}</span>
              <code class="principal">{u.principal}</code>
              <span class="held">{u.admin ? "Admin" : roleText(u)}{u.signed_in ? ", signed in now" : ""}</span>
            </span>
            <span class="act">
              {#if u.you}
              {:else if u.admin}
                <span class="quiet small">Admins can't be acted as</span>
              {:else}
                <Button disabled={!!busy} onclick={() => actAs(u)}>{busy === u.principal ? "Switching…" : "Act as"}</Button>
              {/if}
            </span>
          </li>
        {/each}
      </ul>
    </section>
  {/if}
</div>

<style>
  .sc-page { max-width: 760px; padding: var(--s6) var(--s5); font-family: var(--font-text); }
  h1 { margin: 0 0 var(--s2); font-size: var(--text-h1); font-weight: var(--weight-strong); letter-spacing: -0.01em; }
  h2 { margin: 0 0 var(--s3); font-size: var(--text-h3); font-weight: var(--weight-strong); }
  .lede { margin: 0; max-width: 60ch; color: var(--muted); font-size: var(--text-prose); line-height: var(--leading-prose); }
  .quiet { color: var(--muted); }
  .small { font-size: var(--text-note); }
  .error { color: var(--danger); }
  .link { color: var(--accent); }
  .block { margin-top: var(--s6); }

  .create { display: grid; gap: var(--s4); max-width: 520px; }
  .roles { margin: 0; padding: 0; border: 0; display: grid; gap: var(--s2); }
  .roles legend { padding: 0; margin-bottom: var(--s2); font-size: var(--text-note); font-weight: var(--weight-medium); }
  .role { display: flex; align-items: center; gap: var(--s2); min-height: 36px; cursor: pointer; }
  .role input { width: 18px; height: 18px; accent-color: var(--accent); }
  .role code { font-family: var(--font-mono); font-size: var(--text-small); color: var(--muted); }

  .people { list-style: none; margin: 0; padding: 0; border-top: var(--border) solid var(--line); }
  .person { display: flex; align-items: center; gap: var(--s3); padding: var(--s3) 0; border-bottom: var(--border) solid var(--line); }
  .avatar { display: inline-grid; place-items: center; width: 32px; height: 32px; flex: none; border-radius: var(--radius-pill); background: var(--panel); border: var(--border) solid var(--line-strong); color: var(--muted); font-weight: var(--weight-strong); }
  .who { flex: 1; min-width: 0; display: grid; gap: 2px; }
  .pname { font-weight: var(--weight-medium); color: var(--ink); display: flex; flex-wrap: wrap; align-items: center; gap: var(--s2); }
  .tag { font-size: var(--text-small); font-weight: var(--weight-medium); color: var(--muted); padding: 1px 6px; border: var(--border) solid var(--line-strong); border-radius: var(--radius-xs); }
  .principal { font-family: var(--font-mono); font-size: var(--text-small); color: var(--muted); overflow-wrap: anywhere; }
  .held { font-size: var(--text-note); color: var(--muted); }
  .act { flex: none; }

  @media (max-width: 719.98px) {
    .sc-page { padding: var(--s4); }
    .person { flex-wrap: wrap; }
    .act { width: 100%; padding-left: calc(32px + var(--s3)); }
    .act :global(.sc-button) { width: 100%; }
    .role { min-height: var(--tap); }
  }
</style>
