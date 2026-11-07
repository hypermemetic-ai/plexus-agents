<!-- SC-31: the end of a turn (SUPERCONDUCTOR-27): "Worked for 55s", and the files
     it changed with their +/− when it edited any. A stop says it stopped; a
     failure says what went wrong in plain words and what to do about it. The
     time is the turn's own, less what it spent waiting for you. -->
<script module>
  export const uses = [];

  export function took(ms) {
    const s = Math.max(1, Math.round((ms ?? 0) / 1000));
    if (s < 60) return `${s}s`;
    const m = Math.floor(s / 60);
    if (m < 60) return `${m}m ${s % 60}s`;
    return `${Math.floor(m / 60)}h ${m % 60}m`;
  }

  /** A failure, in the operator's words, with the next step. */
  export function failureWords(failure, detail, host) {
    const where = host ? `on ${host}` : "on the host";
    switch (failure) {
      case "not_logged_in": return [`Claude isn't signed in ${where}.`, `Run claude there, sign in, then send your message again.`];
      case "session_not_found": return ["The host no longer has this conversation.", "Start a new thread in the same folder."];
      case "session_in_use": return ["This conversation is open somewhere else on the host.", "Close it there, then send your message again."];
      case "unsupported_version": return [`The Claude Code ${where} is a version this app can't drive.`, "Update Claude Code there, then send again."];
      case "process_exited": return ["Claude stopped unexpectedly.", "Send a message to carry on from here."];
      case "protocol": return ["Claude answered in a way this app didn't understand.", "Send again; if it keeps happening, update the host."];
      case "agent": return [detail ? `Claude hit an error: ${detail}` : "Claude hit an error.", "Send a message to try again."];
      default: return [detail ? `The turn failed: ${detail}` : "The turn failed.", "Send a message to try again."];
    }
  }
</script>

<script>
  let { foot, host = "" } = $props();
  const failed = $derived(foot.status === "failed");
  const stopped = $derived(foot.status === "interrupted");
  const words = $derived(failed ? failureWords(foot.failure, foot.detail, host) : null);
  const files = $derived(foot.files ?? 0);
</script>

<div class="sc-turnfoot" data-status={foot.status} role="status">
  {#if failed}
    <p class="said"><span class="what">{words[0]}</span> <span class="next">{words[1]}</span></p>
  {:else}
    <p class="said">
      {stopped ? (foot.stopped_by === "deny_and_stop" ? "Stopped when you denied the request, after" : "Stopped after") : "Worked for"} {took(foot.ms)}{#if files}<span class="files">, changed {files} {files === 1 ? "file" : "files"}</span> <span class="add">+{foot.add}</span> <span class="del">−{foot.del}</span>{/if}
    </p>
  {/if}
</div>

<style>
  .sc-turnfoot { display: flex; align-items: center; gap: var(--s3); margin: var(--s3) 0 var(--s5); color: var(--muted); font-size: 12.5px; }
  .sc-turnfoot::after { content: ""; flex: 1; height: 1px; background: var(--line); }
  .said { margin: 0; max-width: 72ch; }
  .add, .del { font-family: var(--font-mono); font-size: 12px; }
  .add { color: var(--diff-add-fg); }
  .del { color: var(--diff-del-fg); }
  .sc-turnfoot[data-status=failed] { align-items: flex-start; }
  .sc-turnfoot[data-status=failed] .said { padding: var(--s2) var(--s3); color: var(--st-failed-fg); background: var(--st-failed-bg); border-radius: var(--radius-md); }
  .sc-turnfoot[data-status=failed]::after { display: none; }
  .what { font-weight: 500; }
  .next { color: var(--ink); }
</style>
