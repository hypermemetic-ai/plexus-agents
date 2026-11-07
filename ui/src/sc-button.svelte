<!-- SC-30: a button. `kind` "primary" (the one action a screen asks for, filled
     cobalt), "quiet" (everything else: a face on the paper) or "danger" (a
     destructive action: brick text, filled only under the pointer). At least
     44px tall on a phone, 36px on a desktop; the focus ring always shows on
     the keyboard. Also `label` (its aria-label, for a button of an icon alone)
     and `title`. -->
<script module>
  export const uses = [];
</script>

<script>
  let { kind = "quiet", type = "button", disabled = false, onclick = undefined, label = undefined, title = undefined, children = undefined } = $props();
</script>

<button class="sc-button" data-surface="sc-button" data-kind={kind} {type} {disabled} {onclick} {title} aria-label={label}>{@render children?.()}</button>

<style>
  .sc-button {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: var(--s2);
    min-height: 36px;
    padding: 0 var(--s4);
    font-family: var(--font-text);
    font-size: var(--text-size);
    font-weight: var(--weight-medium);
    line-height: 1.2;
    white-space: nowrap;
    color: var(--ink);
    background: var(--card);
    border: var(--border) solid var(--line-strong);
    border-radius: var(--radius-sm);
    cursor: pointer;
    transition: background-color var(--motion-fast) var(--ease), border-color var(--motion-fast) var(--ease), color var(--motion-fast) var(--ease);
    -webkit-tap-highlight-color: transparent;
  }
  .sc-button:hover { background: var(--hover); }
  .sc-button:active { transform: translateY(1px); }
  .sc-button[data-kind=primary] { color: var(--on-accent); background: var(--accent); border-color: var(--accent); }
  .sc-button[data-kind=primary]:hover { background: var(--accent-hover); border-color: var(--accent-hover); }
  .sc-button[data-kind=danger] { color: var(--danger); border-color: var(--line-strong); }
  .sc-button[data-kind=danger]:hover { color: var(--on-accent); background: var(--danger); border-color: var(--danger); }
  .sc-button:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }
  .sc-button:disabled, .sc-button:disabled:hover {
    color: var(--disabled);
    background: var(--card);
    border-color: var(--line);
    cursor: not-allowed;
    transform: none;
  }
  @media (max-width: 719.98px) { .sc-button { min-height: var(--tap); } }
  @media (prefers-reduced-motion: reduce) { .sc-button:active { transform: none; } }
</style>
