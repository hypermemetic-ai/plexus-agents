<!-- SC-30: one status per thread (SUPERCONDUCTOR-27, in priority order):
     needs you > working > waiting > failed > done > idle > offline. A lamp and a
     word: only "needs you" is filled — amber, the one loud thing on a screen;
     "working"'s lamp breathes (still under reduced motion); the rest are quiet.
     The same pill in the rail, on a host's page and over a transcript. -->
<script module>
  export const uses = [];
  export const LABEL = { needs: "Needs you", working: "Working", waiting: "Waiting", failed: "Failed", done: "Done", idle: "Idle", offline: "Offline" };
</script>

<script>
  let { status = "idle" } = $props();
  const known = $derived(status in LABEL ? status : "idle");
</script>

<span class="pill" data-surface="sc-pill" data-status={known}><span class="lamp" aria-hidden="true"></span>{LABEL[status] ?? status}</span>

<style>
  .pill {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 22px;
    padding: 0 8px 0 7px;
    font-family: var(--font-text);
    font-size: var(--text-small);
    font-weight: var(--weight-medium);
    line-height: 1;
    white-space: nowrap;
    color: var(--st-idle-fg);
    border-radius: var(--radius-pill);
    flex: none;
  }
  .lamp { width: 7px; height: 7px; border-radius: 50%; background: currentColor; flex: none; }
  .pill[data-status=needs] { color: var(--on-signal); background: var(--signal); font-weight: var(--weight-strong); }
  .pill[data-status=working] { color: var(--st-working-fg); }
  .pill[data-status=working] .lamp { animation: breathe var(--motion-pulse) ease-in-out infinite alternate; }
  .pill[data-status=waiting] { color: var(--st-waiting-fg); }
  .pill[data-status=failed] { color: var(--st-failed-fg); }
  .pill[data-status=done] { color: var(--st-done-fg); }
  .pill[data-status=offline] { color: var(--st-offline-fg); }
  .pill[data-status=offline] .lamp { background: none; box-shadow: inset 0 0 0 1.5px currentColor; }
  @keyframes breathe { from { opacity: 1; transform: scale(1); } to { opacity: .35; transform: scale(.75); } }
  @media (prefers-reduced-motion: reduce) { .lamp { animation: none !important; } }
</style>
