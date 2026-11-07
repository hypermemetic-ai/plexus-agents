<!-- SUPERCONDUCTOR-11: one status per thread (SUPERCONDUCTOR-27's working set, in
     priority order): needs you > working > failed > idle. The same pill in the
     sidebar, on a host's page and over a transcript. -->
<script module>
  export const uses = [];
</script>

<script>
  let { status = "idle" } = $props();
  const LABEL = { needs: "needs you", working: "working", failed: "failed", idle: "idle" };
</script>

<span class="pill" data-status={status}>{LABEL[status] ?? status}</span>

<style>
  .pill { display: inline-block; padding: 0 8px; font-size: 11px; line-height: 18px; border-radius: var(--radius-pill); color: var(--st-idle-fg); background: var(--st-idle-bg); white-space: nowrap; }
  .pill[data-status=needs] { color: var(--st-needs-fg); background: var(--st-needs-bg); }
  .pill[data-status=working] { color: var(--st-working-fg); background: var(--st-working-bg); animation: pulse var(--motion-pulse) var(--ease) infinite alternate; }
  .pill[data-status=failed] { color: var(--st-failed-fg); background: var(--st-failed-bg); }
  @keyframes pulse { from { opacity: 1; } to { opacity: .6; } }
  @media (prefers-reduced-motion: reduce) { .pill { animation: none !important; } }
</style>
