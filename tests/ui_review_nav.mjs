// SC-33: tests/ui_review_nav.rs's browser, one STAGE at a time. Screenshots of the
// nav and the palette at 1280×800 and 390×844 go to $SHOTS as nav-<stage>-<what>.png.
import { createRequire } from "node:module";
import { existsSync } from "node:fs";
const require = createRequire(process.env.NODE_PATH + "/");
const { chromium } = require("playwright");

const { BASE, TOKEN, SHOTS, STAGE, SIGNAL, STALE_THREAD } = process.env;
const browser = await chromium.launch();
const errors = [];
async function open(width, height, hash = "#/") {
  const context = await browser.newContext({ viewport: { width, height }, deviceScaleFactor: width < 720 ? 2 : 1, hasTouch: width < 720 });
  await context.addCookies([{ name: "access_token", value: TOKEN, domain: "127.0.0.1", path: "/" }]);
  const page = await context.newPage();
  page.on("pageerror", (e) => errors.push(String(e)));
  page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
  await page.goto(`${BASE}/${hash}`);
  // Until sc-frame lands, the pages hide the nav on a phone; the frame makes it
  // the whole `#/` screen there, which this stands in for.
  if (width < 720) await page.addStyleTag({ content: "aside.sc-nav { display: flex !important; flex: 1 1 auto !important; } .body > main { display: none !important; } .body { flex: 1; } .page { height: 100dvh; display: flex; flex-direction: column; max-width: 100vw; overflow-x: hidden; } .body { min-width: 0; width: 100%; } header.sc-top { max-width: 100vw; overflow: hidden; }" });
  await page.locator(".sc-nav").first().waitFor({ timeout: 20000 });
  return page;
}
const shot = (page, name) => page.screenshot({ path: `${SHOTS}/nav-${STAGE}-${name}.png` });
const navShot = (page, name) => page.locator(".sc-nav").first().screenshot({ path: `${SHOTS}/nav-${STAGE}-${name}.png` });
const say = (s) => console.log(s);
const fail = (m) => { throw new Error(m); };

process.on("uncaughtException", (e) => { console.log("FAILED:", e.message); console.log("console:", errors.join("\n")); process.exit(1); });
process.on("unhandledRejection", (e) => { console.log("FAILED:", e?.message ?? e); console.log("console:", errors.join("\n")); process.exit(1); });

if (STAGE === "empty") {
  for (const [w, h, tag] of [[1280, 800, "desktop"], [390, 844, "phone"]]) {
    const page = await open(w, h);
    await page.getByText("Pair a machine to start").waitFor({ timeout: 20000 });
    const cmd = await page.locator(".sc-nav .cmd code").textContent();
    if (!/plexus-agents host[\s\\]+--server/.test(cmd)) fail("the empty nav names no command: " + cmd);
    await shot(page, tag);
    await navShot(page, tag + "-nav");
  }
}

if (STAGE === "full") {
  const page = await open(1280, 800);
  // Wait for titles (read from each thread's start) and the needs group.
  await page.locator(".sc-nav .needs .row").first().waitFor({ timeout: 30000 });
  await page.waitForFunction(() => document.querySelectorAll(".sc-nav .row .title:not(.unnamed)").length >= 3, null, { timeout: 30000 }).catch(() => say("note: some titles still unread"));
  await page.waitForTimeout(1500);
  const needs = await page.locator(".sc-nav .needs .row").count();
  const text = await page.locator(".sc-nav").first().textContent();
  say(`needs rows: ${needs}`);
  say("nav text: " + text.replace(/\s+/g, " "));
  for (const h of ["mac", "studio", "laptop"]) if (!text.includes(h)) fail("no " + h + " in the nav");
  if (/[0-9a-f]{8}-[0-9a-f]{4}/.test(text)) fail("a raw thread id in the nav");
  if (!text.includes("Offline")) fail("laptop not shown offline");
  // A thread in Needs you is not listed again below.
  const keys = await page.locator(".sc-nav .row").evaluateAll((as) => as.map((a) => a.dataset.row));
  if (new Set(keys).size !== keys.length) fail("a thread listed twice: " + keys.join(", "));
  await shot(page, "desktop");
  await navShot(page, "desktop-nav");

  // j/k and the keyboard row.
  await page.locator("body").click({ position: { x: 900, y: 400 } });
  await page.keyboard.press("j");
  await page.keyboard.press("j");
  if (!(await page.locator(".sc-nav .row[data-cursor]").count())) fail("j moved no cursor");
  await shot(page, "desktop-keys");
  await page.keyboard.press("Enter");
  await page.waitForURL(/#\/h\/.+\/t\//, { timeout: 5000 });
  await page.waitForTimeout(800);
  if (!(await page.locator(".sc-nav .row[aria-current]").count())) fail("the open thread is not selected in the nav");
  await shot(page, "desktop-selected");

  // The palette: ctrl+k, type, arrows, Enter.
  await page.keyboard.press("Control+k");
  await page.locator(".sc-palette input").waitFor({ timeout: 5000 });
  await page.waitForTimeout(200);
  await shot(page, "palette-desktop");
  await page.keyboard.type("hyperforge");
  await page.waitForTimeout(200);
  const hits = await page.locator(".sc-palette .hit").count();
  say(`palette hits for hyperforge: ${hits}`);
  await shot(page, "palette-desktop-query");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowUp");
  await page.keyboard.press("Enter");
  await page.waitForURL(/#\/h\/studio\/t\//, { timeout: 5000 });
  if (await page.locator(".sc-palette input").count()) fail("the palette stayed open after Enter");
  await page.keyboard.press("Control+k");
  await page.locator(".sc-palette input").waitFor({ timeout: 5000 });
  await page.keyboard.press("Escape");
  await page.waitForTimeout(200);
  if (await page.locator(".sc-palette input").count()) fail("Escape did not close the palette");

  // The phone.
  const phone = await open(390, 844);
  await phone.locator(".sc-nav .needs .row").first().waitFor({ timeout: 30000 });
  await phone.waitForTimeout(1500);
  await shot(phone, "phone");
  await navShot(phone, "phone-nav");
  await phone.locator(".sc-nav .search").first().tap();
  await phone.locator(".sc-palette input").waitFor({ timeout: 5000 });
  await phone.waitForTimeout(200);
  await shot(phone, "palette-phone");
  await phone.keyboard.type("mac");
  await phone.waitForTimeout(200);
  await shot(phone, "palette-phone-query");
  await phone.getByRole("button", { name: "Close" }).tap();

  // The stale pill: the page stays on #/ while studio's approval is answered.
  await page.goto(`${BASE}/#/`);
  const row = page.locator(`.sc-nav .needs .row[data-row="studio/${STALE_THREAD}"]`);
  await row.waitFor({ timeout: 20000 });
  say("waiting for the answer");
  for (let i = 0; !existsSync(SIGNAL); i++) {
    if (i > 900) fail("no answer came in 90 s");
    await new Promise((r) => setTimeout(r, 100));
  }
  const t0 = Date.now();
  await row.waitFor({ state: "detached", timeout: 8000 });
  const after = page.locator(`.sc-nav .row[data-row="studio/${STALE_THREAD}"]`);
  await after.waitFor({ timeout: 8000 });
  const status = await after.getAttribute("data-status");
  say(`studio's thread left Needs you after ${Date.now() - t0} ms, now ${status}`);
  if (status === "needs") fail("studio's thread still shows needs you");
  // And once its turn ends, done (unread) or idle — never needs you again.
  await page.locator(`.sc-nav .row[data-row="studio/${STALE_THREAD}"]:is([data-status=done],[data-status=idle],[data-status=failed])`).waitFor({ timeout: 30000 })
    .then(async () => say("studio's thread ended: " + (await after.getAttribute("data-status"))), () => say("studio's thread still " + status + " after 30 s (the replayed resume)"));
  await page.waitForTimeout(500);
  await shot(page, "after-answer");
}

await browser.close();
const real = errors.filter((e) => !/ui\.watch|favicon|401|403/.test(e));
if (real.length) console.log("console errors:\n" + real.join("\n"));
console.log("ok: " + STAGE);
