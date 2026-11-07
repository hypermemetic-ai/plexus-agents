<!-- stub (SC-34 builder): the contract's sc-pill with its seven statuses; SC-30's
     real one replaces it at merge. -->
<script module>
  export const uses = [];
</script>

<script>
  let { status = "idle" } = $props();
  const LABEL = { needs: "Needs you", working: "Working", waiting: "Waiting", failed: "Failed", done: "Done", idle: "Idle", offline: "Offline" };
</script>

<span class="pill" data-status={status}>{LABEL[status] ?? status}</span>

<style>
  .pill { display: inline-block; padding: 0 var(--s2); font-size: 12px; line-height: 20px; border-radius: var(--radius-pill); white-space: nowrap; color: var(--st-idle-fg); background: var(--st-idle-bg); }
  .pill[data-status=needs] { color: var(--st-needs-fg); background: var(--st-needs-bg); font-weight: 600; }
  .pill[data-status=working] { color: var(--st-working-fg); background: var(--st-working-bg); }
  .pill[data-status=waiting] { color: var(--st-waiting-fg); background: var(--st-waiting-bg); }
  .pill[data-status=failed] { color: var(--st-failed-fg); background: var(--st-failed-bg); }
  .pill[data-status=done] { color: var(--st-done-fg); background: var(--st-done-bg); }
  .pill[data-status=offline] { color: var(--st-offline-fg); background: var(--st-offline-bg); }
</style>
