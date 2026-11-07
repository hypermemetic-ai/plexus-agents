<!-- SC-30's review sheet: tests/ui_review_frame.rs builds a copy of ui/ with this
     as src/sc-sheet.svelte at `#/sheet`, to look at the shared components in
     both schemes and both widths. Never in the shipped bundle. -->
<script module>
  export const uses = [];
</script>

<script>
  import Button from "./sc-button.svelte";
  import Field from "./sc-field.svelte";
  import Pill from "./sc-pill.svelte";
  import Empty from "./sc-empty.svelte";
  import Text from "./sc-text.svelte";

  const TEXT = [
    "I read `src/hosts.rs` and the pairing flow. The confirm step **refuses a host's own key**, so a host can never pair itself; the fix is two lines.",
    "",
    "1. Check the caller before the lookup.",
    "2. Keep the error words the page already shows.",
    "",
    "```rust",
    "fn confirm(&self, caller: &str, id: &str) -> Result<HostsEvent, String> {",
    "    let pending = self.pending.lock().expect(\"hosts\");",
    "    let p = pending.get(id).filter(|p| p.standing()).ok_or(\"no such pairing is pending\")?;",
    "    Ok(HostsEvent::Paired { host_id: p.host_id.clone(), name: p.name.clone() })",
    "}",
    "```",
    "",
    "| File | Lines |",
    "|---|---|",
    "| src/hosts.rs | +2 −1 |",
    "",
    "Run `cargo test --test host_wire` to see it pass. See [the docs](https://example.com/docs).",
  ].join("\n");
  const STATUSES = ["needs", "working", "waiting", "failed", "done", "idle", "offline"];
  let folder = $state("");
</script>

<div class="sheet">
  <h1>Components</h1>
  <section>
    <h2>Status</h2>
    <div class="row">{#each STATUSES as s (s)}<Pill status={s}/>{/each}</div>
  </section>
  <section>
    <h2>Buttons</h2>
    <div class="row">
      <Button kind="primary">Start a thread</Button>
      <Button>Cancel</Button>
      <Button kind="danger">Unpair</Button>
      <Button kind="primary" disabled>Allow once</Button>
    </div>
  </section>
  <section>
    <h2>Fields</h2>
    <div class="fields">
      <Field label="Folder" hint="A folder on the host, where Claude works."><input bind:value={folder} placeholder="/Users/you/project"></Field>
      <Field label="Folder" error="That folder is not on mac. Pick one from the list."><input value="/nope"></Field>
      <Field label="What should Claude do?"><textarea placeholder="Describe the change"></textarea></Field>
    </div>
  </section>
  <section>
    <h2>Text</h2>
    <Text text={TEXT}/>
  </section>
  <section>
    <h2>Empty</h2>
    <Empty title="No hosts yet" body="Pair a machine to run Claude Code on it from here. On the machine, run the command below, then open the link it prints.">
      <Button kind="primary">Pair a host</Button>
    </Empty>
  </section>
</div>

<style>
  .sheet { padding: var(--s5) var(--gutter) var(--s6); overflow-y: auto; }
  h1 { margin: 0 0 var(--s5); font-size: var(--text-h2); font-weight: var(--weight-strong); letter-spacing: -0.01em; }
  h2 { margin: 0 0 var(--s3); font-size: var(--text-note); font-weight: var(--weight-medium); color: var(--muted); }
  section { margin-bottom: var(--s6); }
  .row { display: flex; flex-wrap: wrap; gap: var(--s2); align-items: center; }
  .fields { display: grid; gap: var(--s4); max-width: 480px; }
</style>
