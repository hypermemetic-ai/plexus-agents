<!-- stub (SC-33): sc-pill {status} as the contract names it, until SC-30's lands.
     status ∈ needs working waiting failed done idle offline; one look per status. -->
<script module>
  export const uses = [];
</script>

<script>
  let { status = "idle" } = $props();
  const LABEL = { needs: "Needs you", working: "Working", waiting: "Waiting", failed: "Failed", done: "Done", idle: "Idle", offline: "Offline" };
</script>

<span class="pill" data-status={status}>{LABEL[status] ?? status}</span>

<style>
  .pill { display: inline-block; padding: 0 var(--s2); font-size: 11px; line-height: 18px; border-radius: var(--radius-pill); color: var(--st-idle-fg); background: var(--st-idle-bg); white-space: nowrap; }
  .pill[data-status=needs] { color: var(--st-needs-fg); background: var(--st-needs-bg); font-weight: var(--weight-medium); }
  .pill[data-status=working] { color: var(--st-working-fg); background: var(--st-working-bg); animation: pulse var(--motion-pulse) var(--ease) infinite alternate; }
  .pill[data-status=waiting] { color: var(--st-waiting-fg); background: var(--st-waiting-bg); }
  .pill[data-status=failed] { color: var(--st-failed-fg); background: var(--st-failed-bg); }
  .pill[data-status=done] { color: var(--st-done-fg); background: var(--st-done-bg); }
  .pill[data-status=offline] { color: var(--st-offline-fg); background: var(--st-offline-bg); }
  @keyframes pulse { from { opacity: 1; } to { opacity: .6; } }
  @media (prefers-reduced-motion: reduce) { .pill { animation: none !important; } }
</style>
