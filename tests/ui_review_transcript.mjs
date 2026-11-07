// SC-31: tests/ui_review_transcript.rs's browser. Pairs, starts a thread on the
// scripted run (fixtures/ui-review/transcript), and screenshots the thread's
// transcript at 1280×800 and 390×844: waiting on the approval, then done, then
// with a tool run opened. Fails on console errors and on sideways scroll at 390.
import { createRequire } from "node:module";
const require = createRequire(process.env.NODE_PATH + "/");
const { chromium } = require("playwright");

const { BASE, LINK, TOKEN, SHOTS } = process.env;
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
  try { await page.screenshot({ path: `${SHOTS}/failed.png`, fullPage: true }); console.log("main:", await page.locator("body").textContent()); } catch {}
  console.log("console:", errors.join("\n"));
  process.exit(1);
});

async function both(name, prep) {
  for (const [w, h] of [[1280, 800], [390, 844]]) {
    await page.setViewportSize({ width: w, height: h });
    await page.waitForTimeout(250);
    if (prep) await prep(w);
    await page.screenshot({ path: `${SHOTS}/${name}-${w}.png` });
    await page.screenshot({ path: `${SHOTS}/${name}-${w}-full.png`, fullPage: true });
    const wide = await page.evaluate(() => {
      const d = document.documentElement;
      if (d.scrollWidth <= d.clientWidth + 1) return null;
      const over = [...document.querySelectorAll(".sc-thread *")].filter((e) => e.getBoundingClientRect().right > d.clientWidth + 1).slice(0, 5);
      return over.map((e) => e.className || e.tagName).join(", ");
    });
    if (wide) throw new Error(`${name} at ${w}px scrolls sideways: ${wide}`);
  }
  await page.setViewportSize({ width: 1280, height: 800 });
}

step("pair, and start a thread on the scripted run");
await page.goto(LINK);
await page.getByRole("button", { name: "Pair", exact: true }).click({ timeout: 20000 });
await page.getByText("is paired").waitFor({ timeout: 20000 });
await page.goto(`${BASE}/#/h/mac/new`);
await page.getByLabel("Folder path").waitFor({ timeout: 30000 });
await page.getByLabel("Folder path").fill("/home/op/dev/lantern");
await page.getByPlaceholder("What should Claude do?").fill("The parser tests fail on bracketed log levels. Find out why, fix it, and run the whole suite.");
await page.getByRole("button", { name: "Start thread" }).click();
await page.waitForURL(/#\/h\/mac\/t\//, { timeout: 30000 });

step("the transcript, waiting on the approval");
await page.getByRole("button", { name: "Allow once" }).waitFor({ timeout: 60000 });
await page.waitForTimeout(500);
await both("thread-approval");
const text = await page.locator(".sc-thread").textContent();
for (const bad of ["toolu_", "[runtime]", "parked"]) if (text.includes(bad)) throw new Error(`the transcript shows ${bad}`);

step("allowed, the turn ends");
await page.getByRole("button", { name: "Allow once" }).click();
await page.locator(".sc-turnfoot").first().waitFor({ timeout: 60000 });
await page.waitForTimeout(500);
await both("thread-done");
const done = await page.locator(".sc-thread").textContent();
for (const bad of ["toolu_", "[runtime]", "parked"]) if (done.includes(bad)) throw new Error(`the transcript shows ${bad}`);
console.log("transcript:", done.replace(/\s+/g, " ").slice(0, 2000));

step("a tool run opened, and a command's output");
await both("thread-open", async () => {
  const runs = page.locator(".sc-toolrun > .summary[aria-expanded=false]");
  while (await runs.count()) await runs.first().click();
  const failed = page.locator('.tool[data-kind=command][data-state=failed] .row[aria-expanded=false]');
  if (await failed.count()) await failed.first().click();
  await page.locator(".sc-toolrun").first().scrollIntoViewIfNeeded();
});

step("scrolled up, the way back to the latest");
await page.evaluate(() => (document.querySelector(".sc-thread .scroller").scrollTop = 0));
await page.waitForTimeout(200);
await page.getByRole("button", { name: "Jump to latest" }).waitFor({ timeout: 5000 });
await page.screenshot({ path: `${SHOTS}/thread-jump-1280.png` });

await browser.close();
const real = errors.filter((e) => !/ui\.watch|favicon|401|403/.test(e));
if (real.length) throw new Error("console errors: " + real.join("\n"));
console.log("ok: the transcript reviewed at 1280 and 390");
