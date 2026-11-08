// SUPERCONDUCTOR UI history: tests/ui_review_history.rs's browser. Pairs, opens the
// long thread the host's store holds, counts what crossed the socket until the
// newest prompt is drawn, then scrolls to the top again and again: each older page
// lands above without moving what was in sight, until the first prompt. With
// BEFORE=1 (a bundle from before the history) it only opens and counts.
import { createRequire } from "node:module";
const require = createRequire(process.env.NODE_PATH + "/");
const { chromium } = require("playwright");

const { BASE, LINK, TOKEN, SHOTS, THREAD } = process.env;
const BEFORE = process.env.SC_UI_HISTORY_BEFORE === "1";
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
await context.addCookies([{ name: "access_token", value: TOKEN, domain: "127.0.0.1", path: "/" }]);
const page = await context.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
const step = (s) => console.log("step:", s);
process.on("uncaughtException", async (e) => {
  console.log("FAILED:", e.message);
  try { await page.screenshot({ path: `${SHOTS}/failed.png`, fullPage: true }); } catch {}
  console.log("console:", errors.join("\n"));
  process.exit(1);
});

// Every frame the page's socket receives, from when the thread is asked.
let counting = false, frames = 0, bytes = 0;
page.on("websocket", (ws) => ws.on("framereceived", (f) => {
  if (!counting) return;
  frames += 1;
  bytes += typeof f.payload === "string" ? Buffer.byteLength(f.payload) : f.payload.length;
}));

step("pair");
await page.goto(LINK);
await page.getByRole("button", { name: "Pair", exact: true }).click({ timeout: 20000 });
await page.getByText("is paired").waitFor({ timeout: 20000 });
await page.goto(`${BASE}/#/h/mac`);
await page.waitForTimeout(1500);

step("open the long thread");
counting = true;
const t0 = Date.now();
await page.goto(`${BASE}/#/h/mac/t/${THREAD}`);
await page.locator(".sc-thread .user", { hasText: "Prompt 30:" }).waitFor({ timeout: 60000 });
const firstRender = Date.now() - t0;
await page.waitForTimeout(1500);     // whatever else the open sends
counting = false;
const users = await page.locator(".sc-thread .user").count();
console.log(`open: ${frames} frames, ${bytes} bytes received; newest prompt drawn after ${firstRender} ms; ${users} prompts drawn`);
await page.screenshot({ path: `${SHOTS}/history-open${BEFORE ? "-before" : ""}.png` });
if (BEFORE) { await browser.close(); process.exit(0); }
if (users >= 30) throw new Error("the whole thread was drawn at once: no paging");

step("scroll back, page by page");
const sc = page.locator(".sc-thread .scroller");
for (let i = 0; i < 30; i++) {
  if (await page.locator(".sc-thread .user", { hasText: "Prompt 1:" }).count()) break;
  // What's in sight at the top, and where, before the older page lands.
  const before = await sc.evaluate((el) => {
    el.scrollTop = 0;
    const top = el.getBoundingClientRect().top;
    const it = [...el.querySelector(".log").children].find((c) => c.getBoundingClientRect().bottom > top + 1);
    it.dataset.watch = "1";
    return { y: it.getBoundingClientRect().top - top, n: el.querySelectorAll(".log > *").length };
  });
  await page.waitForFunction((n) => document.querySelectorAll(".sc-thread .log > *").length > n, before.n, { timeout: 20000 });
  await page.waitForTimeout(300);
  const after = await sc.evaluate((el) => {
    const w = el.querySelector("[data-watch]");
    const y = w ? w.getBoundingClientRect().top - el.getBoundingClientRect().top : null;
    if (w) delete w.dataset.watch;
    return y;
  });
  if (after == null) throw new Error("the item in sight was redrawn, not kept");
  if (Math.abs(after - before.y) > 2) throw new Error(`the view moved ${after - before.y}px when an older page landed`);
  console.log(`page ${i + 1}: ${await page.locator(".sc-thread .user").count()} prompts; the view held (${before.y.toFixed(1)} → ${after.toFixed(1)})`);
}
const all = await page.locator(".sc-thread .user").count();
if (all !== 30) throw new Error(`scrolled back to ${all} prompts, not 30`);
if (!(await page.locator(".sc-thread .user", { hasText: "Prompt 1:" }).count())) throw new Error("never reached the first prompt");
if (await page.locator(".sc-thread .older").count()) throw new Error("still offers an older page at the start");
const text = await page.locator(".sc-thread").textContent();
for (const bad of ["toolu_", "[runtime]"]) if (text.includes(bad)) throw new Error(`the transcript shows ${bad}`);
await page.screenshot({ path: `${SHOTS}/history-start.png` });

const real = errors.filter((e) => !/ui\.watch|favicon|401|403/.test(e));
if (real.length) throw new Error("console errors:\n" + real.join("\n"));
console.log("ok");
await browser.close();
