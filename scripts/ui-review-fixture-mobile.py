"""Mobile: writes fixtures/ui-review/mobile — scripts/ui-review-fixture.py's run made
long and wide for a phone (tests/ui_review_mobile.rs): a long code block with
lines wider than the screen, a wide table, an unbroken path, a 260-line command
output, and after the approval several replies that arrive while the reader is
scrolled up. Derived from SC-31's generator:
 — a scripted Claude run for the
transcript's review harness, built from restart-parked's real frames (its init,
its parked control_request, its result) with richer content between: prose with
markdown and a code block, reads, searches, commands (one failing), a TodoWrite,
an Edit and a Write, a parked approval, and a finished turn.

    python3 gen_fixture.py <repo>
"""
import copy, json, os, sys

repo = sys.argv[1]
src = os.path.join(repo, "fixtures/recorded/agents-9-2026-11-01/restart-parked")
out = os.path.join(repo, "fixtures/ui-review/mobile")
S = "af9c8cb7-9d7a-4b6b-9753-95decd27b4f6"


def frames(phase):
    with open(os.path.join(src, phase, "1-claude.stdout")) as f:
        return [json.loads(l) for l in f if l.strip()]


svc1, svc2 = frames("svc1"), frames("svc2")
init1, init2 = svc1[0], svc2[0]
for i in (init1, init2):
    i["cwd"] = i["original_cwd"] = "/home/op/dev/lantern"
    i["tools"] = ["Bash", "Read", "Grep", "Glob", "Edit", "Write", "TodoWrite"]
result = copy.deepcopy(svc2[-1])
n = [0]


def mid():
    n[0] += 1
    return f"msg_review{n[0]:04d}"


def text(t, chunks=6):
    m = mid()
    ev = lambda e: {"type": "stream_event", "event": e, "session_id": S, "parent_tool_use_id": None, "api_message_id": m}
    out = [ev({"type": "message_start", "message": {"model": "claude-haiku-5-5", "id": m, "type": "message", "role": "assistant", "content": []}}),
           ev({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})]
    step = max(1, len(t) // chunks)
    for k in range(0, len(t), step):
        out.append(ev({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": t[k:k + step]}}))
    out.append({"type": "assistant", "message": {"model": "claude-haiku-5-5", "id": m, "type": "message", "role": "assistant", "content": [{"type": "text", "text": t}]}, "parent_tool_use_id": None, "session_id": S})
    out.append(ev({"type": "content_block_stop", "index": 0}))
    out.append(ev({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}))
    out.append(ev({"type": "message_stop"}))
    return out


def call(tid, name, inp):
    return {"type": "assistant", "message": {"model": "claude-haiku-5-5", "id": mid(), "type": "message", "role": "assistant", "content": [{"type": "tool_use", "id": tid, "name": name, "input": inp}]}, "parent_tool_use_id": None, "session_id": S}


def res(tid, content, err=False):
    return {"type": "user", "message": {"role": "user", "content": [{"tool_use_id": tid, "type": "tool_result", "content": content, "is_error": err}]}, "parent_tool_use_id": None, "session_id": S}


def tool(tid, name, inp, content, err=False):
    return [call(tid, name, inp), res(tid, content, err)]


F = "/home/op/dev/lantern"
old_fn = "pub fn parse(line: &str) -> Option<Entry> {\n    let mut parts = line.splitn(3, ' ');\n    let at = parts.next()?;\n    let level = parts.next()?;\n    let msg = parts.next()?;\n    Some(Entry { at: at.into(), level: level.into(), msg: msg.into() })\n}"
new_fn = "pub fn parse(line: &str) -> Option<Entry> {\n    let mut parts = line.splitn(3, ' ');\n    let at = parts.next()?;\n    let level = parts.next()?.trim_matches(|c| c == '[' || c == ']');\n    let msg = parts.next().unwrap_or(\"\");\n    Some(Entry { at: at.into(), level: level.to_ascii_lowercase(), msg: msg.trim_end().into() })\n}"
test_out_fail = "Exit code 101\n   Compiling lantern v0.3.1 (/home/op/dev/lantern)\n    Finished `test` profile [unoptimized + debuginfo] target(s) in 2.41s\n     Running unittests src/lib.rs\n\nrunning 6 tests\ntest parse::tests::plain_line ... ok\ntest parse::tests::bracketed_level ... FAILED\ntest parse::tests::empty_message ... FAILED\ntest tail::tests::follows ... ok\ntest tail::tests::rotates ... ok\ntest filter::tests::by_level ... ok\n\nfailures:\n\n---- parse::tests::bracketed_level stdout ----\nthread 'parse::tests::bracketed_level' panicked at src/parse.rs:41:9:\nassertion `left == right` failed\n  left: \"[WARN]\"\n right: \"warn\"\n\nfailures:\n    parse::tests::bracketed_level\n    parse::tests::empty_message\n\ntest result: FAILED. 4 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s"

p1 = [init1]
p1 += text("I'll look at how log lines are parsed first, then run the tests to see what fails.")
p1 += tool("toolu_r01", "TodoWrite", {"todos": [
    {"content": "Read the parser and its tests", "activeForm": "Reading the parser", "status": "in_progress"},
    {"content": "Fix bracketed levels and empty messages", "activeForm": "Fixing the parser", "status": "pending"},
    {"content": "Run the test suite", "activeForm": "Running the tests", "status": "pending"}]}, "Todos have been modified successfully.")
p1 += tool("toolu_r02", "Glob", {"pattern": "src/**/*.rs"}, f"{F}/src/lib.rs\n{F}/src/parse.rs\n{F}/src/tail.rs\n{F}/src/filter.rs")
p1 += tool("toolu_r03", "Read", {"file_path": f"{F}/src/parse.rs"}, "     1\tuse crate::Entry;\n     2\t\n     3\t" + old_fn.replace("\n", "\n     4\t"))
p1 += tool("toolu_r04", "Grep", {"pattern": "fn parse", "path": f"{F}/src", "output_mode": "content"}, "src/parse.rs:3:pub fn parse(line: &str) -> Option<Entry> {")
p1 += tool("toolu_r05", "Read", {"file_path": f"{F}/src/lib.rs"}, "     1\tpub mod parse;\n     2\tpub mod tail;\n     3\tpub mod filter;")
p1 += tool("toolu_r06", "Bash", {"command": "cargo test --quiet 2>&1 | tail -n 40", "description": "Run the test suite"}, test_out_fail, True)
p1 += text(
    "Two tests fail, both in `parse`:\n\n"
    "1. **Bracketed levels** like `[WARN]` keep their brackets, so `bracketed_level` sees `\"[WARN]\"` instead of `\"warn\"`.\n"
    "2. **Empty messages** return `None`, because `parts.next()?` bails when a line has no message.\n\n"
    "The fix is small: trim the brackets, lower-case the level, and treat a missing message as empty.\n\n"
    "```rust\nlet level = parts.next()?.trim_matches(|c| c == '[' || c == ']');\nlet msg = parts.next().unwrap_or(\"\");\n```\n\n"
    "I'll make that change now.")
p1 += tool("toolu_r07", "Edit", {"file_path": f"{F}/src/parse.rs", "old_string": old_fn, "new_string": new_fn}, f"The file {F}/src/parse.rs has been updated successfully.")
p1 += tool("toolu_r08", "Write", {"file_path": f"{F}/CHANGELOG.md", "content": "# Changelog\n\n## 0.3.2\n\n- Parse levels written in brackets, like `[WARN]`.\n- A line with no message parses as an empty message.\n"}, f"File created successfully at: {F}/CHANGELOG.md")
long_code = "\n".join([
    "use std::collections::HashMap;",
    "",
    "/// Every level the parser has seen, with how often, for the summary the CLI prints at the end of a tail session.",
    "pub fn summarize(entries: &[Entry]) -> HashMap<String, usize> {",
    "    let mut counts: HashMap<String, usize> = HashMap::with_capacity(entries.len().min(16));",
    "    for e in entries.iter().filter(|e| !e.level.is_empty() && e.level != \"trace\" && e.level != \"debug\") {",
    "        *counts.entry(e.level.clone()).or_insert(0) += 1;",
    "    }",
    "    counts",
    "}",
] + [f"// line {k}: padding so the block is long enough to scroll on a phone" for k in range(1, 31)])
p1 += text("Before I run everything, here is where the levels are counted, so you can see what else reads `level`:\n\n"
           "```rust\n" + long_code + "\n```\n\n"
           "| Reader | Where | What it expects from the level field after this change lands |\n|---|---|---|\n"
           "| `summarize` | `src/summary.rs` | lower-case, brackets gone, never empty when a level was written |\n"
           "| `by_level` | `src/filter.rs` | the same, compared with `eq_ignore_ascii_case` today |\n\n"
           "The log I tested with lives at `/home/op/dev/lantern/fixtures/logs/2026-11-07T09-14-22.551Z-production-eu-west-1-ingest-worker-7f9c2d.log`.")
long_out = "\n".join(f"test parse::fuzz::case_{k:04d} ... ok" for k in range(1, 261))
p1 += tool("toolu_r09", "Bash", {"command": "cargo test --quiet fuzz -- --nocapture", "description": "Run the fuzz cases"}, long_out)
p1 += text("Now I'll run the whole suite again, including the slow integration tests.")
# The parked approval: restart-parked's own frames, with this run's command.
for f in svc1[1:]:
    f = copy.deepcopy(f)
    if f.get("type") == "assistant":
        f["message"]["content"][0]["input"] = {"command": "cargo test --all-features -- --include-ignored", "description": "Run every test, slow ones included"}
    if f.get("type") == "control_request":
        f["request"]["input"] = {"command": "cargo test --all-features -- --include-ignored", "description": "Run every test, slow ones included"}
        f["request"]["description"] = "Run every test, slow ones included"
        f["request"]["permission_suggestions"][0]["rules"][0]["ruleContent"] = "cargo test:*"
    if f.get("type") == "stream_event" and f["event"].get("type") == "content_block_delta":
        continue
    p1.append(f)

p2 = [init2]
cmd = {"command": "cargo test --all-features -- --include-ignored", "description": "Run every test, slow ones included"}
upto = next(i for i, f in enumerate(svc2) if f.get("type") == "user")
for f in svc2[1:upto + 1]:
    f = copy.deepcopy(f)
    if f.get("type") == "stream_event" and f["event"].get("type") == "content_block_delta":
        continue
    if f.get("type") == "assistant":
        f["message"]["content"][0]["input"] = cmd
    if f.get("type") == "control_request":
        f["request"]["input"] = cmd
    if f.get("type") == "user":
        f["message"]["content"][0]["content"] = "running 6 tests\n......\ntest result: ok. 6 passed; 0 failed; 0 ignored\n\nrunning 3 tests (integration)\n...\ntest result: ok. 3 passed; 0 failed; 0 ignored"
    p2.append(f)
p2 += tool("toolu_r10", "TodoWrite", {"todos": [
    {"content": "Read the parser and its tests", "activeForm": "Reading the parser", "status": "completed"},
    {"content": "Fix bracketed levels and empty messages", "activeForm": "Fixing the parser", "status": "completed"},
    {"content": "Run the test suite", "activeForm": "Running the tests", "status": "completed"}]}, "Todos have been modified successfully.")
for k, said in enumerate([
    "The integration tests are slow because each one starts a real tailer; that is expected.",
    "Nothing else reads the raw level, so no other module needs to change.",
    "I also checked the CLI's `--level` flag: it already lower-cases what you pass.",
]):
    p2 += text(said)
p2 += text("All **9 tests** pass now, the slow integration tests included.\n\n"
           "| Change | File |\n|---|---|\n| Levels in brackets parse, lower-cased | `src/parse.rs` |\n| Missing message is empty, not an error | `src/parse.rs` |\n| Release note | `CHANGELOG.md` |\n\n"
           "Want me to bump the version to `0.3.2` and tag it?")
result["result"] = "All 9 tests pass now."
p2.append(result)

for phase, fr in (("svc1", p1), ("svc2", p2)):
    d = os.path.join(out, phase)
    os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, "1-claude.stdout"), "w") as f:
        for x in fr:
            f.write(json.dumps(x) + "\n")
print("wrote", out, len(p1), len(p2))
