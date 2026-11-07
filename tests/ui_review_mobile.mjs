// Mobile: tests/ui_review_mobile.rs's browser. Playwright WebKit at 390×844 with an
// iPhone's touch and agent, and menger's keyboard shim (tests/kb-shim.js: a 336px
// keyboard that shrinks and pans the visual viewport as iOS does). Pairs, starts a
// thread on fixtures/ui-review/mobile and checks, as menger's kb-check,
// composer-inset-check and chat-anchor-check did:
//   - the thread opens at its newest; nothing scrolls sideways; a wide code block
//     scrolls inside its card;
//   - a tap on the field raises the keyboard and the composer's bottom touches the
//     keyboard's top, the thread's head at the top of what is seen;
//   - tapping Send keeps the field focused and the keyboard up;
//   - scrolled up, replies that arrive move nothing; the ↓ shows with their count,
//     and takes the reader back to the end.
// Screenshots to $SHOTS: mobile-approval, mobile-code, mobile-keyboard,
// mobile-scrolled-up, mobile-done. Exit 1 on a failure.
import { createRequire } from "node:module";
import fs from "node:fs";
const require = createRequire(process.env.NODE_PATH + "/");
const pw = require("playwright");

const { BASE, LINK, TOKEN, SHOTS } = process.env;
const shim = fs.readFileSync(new URL("./kb-shim.js", import.meta.url), "utf8");
// WebKit where the host can run it; else Chromium with the same phone emulation
// (said in the output: the engine differs, the keyboard model is the same shim).
let browser;
try { browser = await pw.webkit.launch(); console.log("engine: webkit"); }
catch (e) {
  console.log("engine: chromium (WebKit cannot start here: " + String(e.message).split("\n").find((l) => /missing|install/i.test(l))?.trim() + ")");
  browser = await pw.chromium.launch();
}
const context = await browser.newContext({ ...pw.devices["iPhone 13"], viewport: { width: 390, height: 844 }, screen: { width: 390, height: 844 } });
await context.addCookies([{ name: "access_token", value: TOKEN, domain: "127.0.0.1", path: "/" }]);
await context.addInitScript(`window.__kbConf = { k: 336, ms: 300 };\n${shim}`);
const page = await context.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
const fails = [];
const ok = (cond, what) => { if (!cond) fails.push(what); console.log(`${cond ? "ok  " : "FAIL"} ${what}`); };
const step = (s) => console.log("step:", s);
const r0 = (n) => (n == null ? "-" : Math.round(n));
process.on("uncaughtException", async (e) => {
  console.log("FAILED:", e.message);
  try { await page.screenshot({ path: `${SHOTS}/mobile-failed.png` }); } catch {}
  console.log("console:", errors.join("\n"));
  process.exit(1);
});

// The phone's screen, with the shim's keyboard drawn under the true visible area.
async function shot(name) {
  await page.evaluate(() => {
    const s = window.__kb.state();
    for (const e of document.querySelectorAll(".kbshim-draw")) e.remove();
    if (s.kb < 1) return;
    const kb = document.createElement("div");
    kb.className = "kbshim-draw";
    kb.style.cssText = `position:fixed;left:0;right:0;top:${s.bottom}px;height:${s.kb}px;background:#3a3a3c;color:#fff;font:14px sans-serif;display:grid;place-items:center;pointer-events:none;z-index:2147483647`;
    kb.textContent = "keyboard";
    document.documentElement.append(kb);
  });
  await page.screenshot({ path: `${SHOTS}/${name}.png` });
  await page.evaluate(() => { for (const e of document.querySelectorAll(".kbshim-draw")) e.remove(); });
}
const sideways = (tag) => page.evaluate(() => {
  const d = document.documentElement, s = document.querySelector(".sc-thread .scroller");
  const over = [...document.querySelectorAll(".sc-thread *")]
    .filter((e) => e.getBoundingClientRect().right > d.clientWidth + 1 && !e.closest(".scroll, .table-scroll, .lines, .code-card"))
    .slice(0, 4).map((e) => e.className || e.tagName);
  return { doc: d.scrollWidth - d.clientWidth, list: s ? s.scrollWidth - s.clientWidth : 0, over };
}).then((w) => ok(w.doc <= 1 && w.list <= 1 && !w.over.length, `${tag}: nothing scrolls sideways (page +${w.doc}, transcript +${w.list}${w.over.length ? ", wider: " + w.over.join(", ") : ""})`));
const look = () => page.evaluate(() => {
  const r = (s) => { const e = document.querySelector(s); if (!e) return null; const b = e.getBoundingClientRect(); return { top: b.top, bottom: b.bottom }; };
  const sc = document.querySelector(".sc-thread .scroller");
  return {
    kb: window.__kb.state(), dataKb: document.documentElement.hasAttribute("data-kb"),
    composer: r("[data-surface=sc-composer]"), head: r(".sc-thread .head"),
    end: sc ? sc.scrollHeight - sc.scrollTop - sc.clientHeight : null, top: sc?.scrollTop ?? null,
    focused: document.activeElement?.tagName === "TEXTAREA" && !!document.activeElement.closest("[data-surface=sc-composer]"),
    jump: !!document.querySelector(".sc-thread .jump"), count: document.querySelector(".sc-thread .jump .count")?.textContent ?? "",
    fontSize: parseFloat(getComputedStyle(document.querySelector("[data-surface=sc-composer] textarea")).fontSize),
  };
});

step("pair, and start a thread on the scripted run");
await page.goto(LINK);
await page.getByRole("button", { name: "Pair", exact: true }).click({ timeout: 20000 });
await page.getByText("is paired").waitFor({ timeout: 20000 });
await page.goto(`${BASE}/#/h/mac/new`);
await page.getByLabel("Folder path").waitFor({ timeout: 30000 });
await page.getByPlaceholder("What should Claude do?").fill("The parser tests fail on bracketed log levels. Find out why, fix it, and run the whole suite.");
await page.getByLabel("Folder path").fill("/home/op/dev/lantern");
await page.getByRole("button", { name: "Start thread" }).click();
await page.waitForURL(/#\/h\/mac\/t\//, { timeout: 30000 });
await page.evaluate(() => document.activeElement?.blur());

step("waiting on the approval: opened at the newest");
await page.getByRole("button", { name: "Allow once" }).waitFor({ timeout: 60000 });
await page.waitForTimeout(1200);
let v = await look();
ok(v.end != null && v.end < 48, `the thread opens at its newest (${r0(v.end)}px from the end)`);
ok(v.fontSize < 16, `the field's text stays small (${v.fontSize}px)`);
await sideways("approval");
await shot("mobile-approval");

step("a long code block");
const code = await page.evaluate(() => {
  const card = [...document.querySelectorAll(".sc-thread .code-card")].find((c) => /summarize/.test(c.textContent));
  if (!card) return null;
  card.scrollIntoView({ block: "start" });
  const sc = card.querySelector(".scroll");
  const b = card.getBoundingClientRect();
  return { inner: sc ? sc.scrollWidth - sc.clientWidth : 0, fits: b.right <= document.documentElement.clientWidth + 1, lang: card.querySelector(".lang")?.textContent ?? "" };
});
ok(code && code.fits, `a wide code block stays inside its card, wrapped or scrolling (+${code?.inner}px inside, language "${code?.lang}")`);
await page.waitForTimeout(300);
await sideways("code");
await shot("mobile-code");

step("the keyboard: tap the field");
await page.evaluate(() => { const s = document.querySelector(".sc-thread .scroller"); s.scrollTop = s.scrollHeight; });
await page.waitForTimeout(300);
await page.tap("[data-surface=sc-composer] textarea");
await page.waitForTimeout(1200);
v = await look();
ok(v.dataKb, "data-kb is set with the keyboard up");
ok(v.composer && Math.abs(v.composer.bottom - v.kb.bottom) <= 2, `the composer's bottom touches the keyboard's top (${r0(v.composer?.bottom)} vs ${r0(v.kb.bottom)})`);
ok(v.composer && v.composer.top >= v.kb.top, `the composer is inside what is seen (${r0(v.composer?.top)} ≥ ${r0(v.kb.top)})`);
ok(v.head && Math.abs(v.head.top - v.kb.top) <= 2, `the thread's head is at the top of what is seen (${r0(v.head?.top)} vs ${r0(v.kb.top)})`);
ok(v.end < 48, `the newest stays above the composer (${r0(v.end)}px from the end)`);
await page.keyboard.type("Also check the CLI flag.");
await page.waitForTimeout(200);
await sideways("keyboard");
await shot("mobile-keyboard");

step("Send keeps the keyboard up");
await page.tap("[data-surface=sc-composer] .go button");
await page.waitForTimeout(800);
v = await look();
ok(v.focused, "the field keeps its focus after Send");
ok(v.dataKb, "the keyboard stays up after Send");
const queued = await page.locator("[data-surface=sc-composer] .queued").count();
ok(queued === 1, `the follow-up was queued (Claude is waiting): ${queued}`);
await page.locator("[data-surface=sc-composer] .queued").getByRole("button", { name: "Remove" }).tap();
await page.waitForTimeout(1200);
v = await look();
ok(!v.dataKb, "another control acts, and the keyboard goes");

step("scrolled up: replies arrive, nothing moves");
await page.evaluate(() => document.activeElement?.blur());
await page.mouse.move(195, 400);
await page.mouse.wheel(0, -100000);
await page.waitForTimeout(400);
v = await look();
ok(v.jump, "the ↓ shows more than a screen above the end");
const before = await page.evaluate(() => { const f = document.querySelector(".sc-thread .log > *:nth-child(2)"); return f.getBoundingClientRect().top; });
await page.evaluate(() => [...document.querySelectorAll("button")].find((b) => b.textContent.includes("Allow once")).click());
await page.locator(".sc-turnfoot").first().waitFor({ state: "attached", timeout: 60000 });
await page.waitForTimeout(1000);
v = await look();
const after = await page.evaluate(() => document.querySelector(".sc-thread .log > *:nth-child(2)").getBoundingClientRect().top);
ok(Math.abs(after - before) <= 2, `the view did not move as replies arrived (${r0(before)} → ${r0(after)}, scrollTop ${r0(v.top)})`);
ok(v.jump && Number(v.count.replace("+", "")) > 0, `the ↓ counts what arrived (${v.count})`);
await sideways("scrolled-up");
await shot("mobile-scrolled-up");

step("↓ back to the latest");
await page.tap(".sc-thread .jump");
await page.waitForTimeout(500);
v = await look();
ok(v.end < 48 && !v.jump, `the ↓ lands at the end and goes (${r0(v.end)}px)`);
await shot("mobile-done");

await browser.close();
const real = errors.filter((e) => !/ui\.watch|favicon|401|403/.test(e));
ok(!real.length, "no console errors" + (real.length ? ": " + real.join("\n") : ""));
if (fails.length) { console.log("FAILED:\n" + fails.join("\n")); process.exit(1); }
console.log("ok: the thread on a phone, keyboard and all");
