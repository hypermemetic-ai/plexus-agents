// SUPERCONDUCTOR-34: tests/ui_review_pages.rs's browser. Walks the pages build as
// a person would and screenshots each screen at desktop and phone size into
// $SHOTS (NAME-desktop.png, NAME-phone.png), checking the phone has no
// horizontal scroll.
import { createRequire } from "node:module";
const require = createRequire(process.env.NODE_PATH + "/");
const { chromium } = require("playwright");

const { BASE, LINK, CODE, TOKEN, SHOTS, CWD_A, CWD_B } = process.env;
const DESKTOP = { width: 1280, height: 800 };
const PHONE = { width: 390, height: 844 };
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: DESKTOP });
await context.addCookies([{ name: "access_token", value: TOKEN, domain: "127.0.0.1", path: "/" }]);
const page = await context.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
const step = (s) => console.log("step:", s);
process.on("uncaughtException", async (e) => {
  console.log("FAILED:", e.message);
  try { await page.screenshot({ path: `${SHOTS}/failed.png` }); console.log("text:", await page.locator("body").textContent()); } catch {}
  console.log("console:", errors.join("\n"));
  process.exit(1);
});
async function shots(name) {
  await page.waitForTimeout(300);
  await page.screenshot({ path: `${SHOTS}/${name}-desktop.png` });
  await page.setViewportSize(PHONE);
  await page.waitForTimeout(300);
  const wide = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  if (wide > 0) console.log(`WARN: ${name} scrolls sideways by ${wide}px on a phone`);
  await page.screenshot({ path: `${SHOTS}/${name}-phone.png`, fullPage: true });
  await page.setViewportSize(DESKTOP);
}

step("home, no machines");
await page.goto(`${BASE}/#/`);
await page.getByText("Connect your first machine").waitFor({ timeout: 20000 });
await shots("1-home-empty");

step("pair landing");
await page.goto(`${BASE}/#/pair`);
await page.getByRole("heading", { name: "Pair a machine" }).waitFor({ timeout: 20000 });
await shots("2-pair");

step("pair by the link");
await page.goto(LINK);
await page.getByText("Does your terminal show this code?").waitFor({ timeout: 20000 });
const shown = await page.locator(".code").textContent();
if (shown.trim() !== CODE.trim()) throw new Error(`the page shows ${shown}, the host ${CODE}`);
await shots("3-pair-confirm");
await page.getByRole("button", { name: "Pair", exact: true }).click();
await page.getByText("is paired").waitFor({ timeout: 20000 });
await shots("4-paired");

step("the machine, no threads yet");
await page.goto(`${BASE}/#/h/mac`);
await page.getByText("Online now").waitFor({ timeout: 30000 });
await shots("5-host-empty");

step("a first thread, in a folder typed in");
await page.goto(`${BASE}/#/h/mac/new`);
await page.getByLabel("Folder path").waitFor({ timeout: 30000 });
await page.getByPlaceholder("What should Claude do?").fill("Tidy up the README and check the build still passes.");
await page.getByLabel("Folder path").fill(CWD_A);
await shots("6-new-first");
await page.getByRole("button", { name: "Start thread" }).click();
await page.waitForURL(/#\/h\/mac\/t\//, { timeout: 30000 });

step("a second thread: the first folder offered, another typed, Ctrl+Enter");
await page.goto(`${BASE}/#/h/mac/new`);
await page.getByRole("radio", { name: /superconductor/ }).waitFor({ timeout: 30000 });
await page.getByPlaceholder("What should Claude do?").fill("Why does the sidebar flicker when a thread starts?");
await shots("7-new-recent");
await page.getByRole("radio", { name: "Other folder…" }).click();
await page.getByLabel("Folder path").fill(CWD_B);
await page.getByPlaceholder("What should Claude do?").press("Control+Enter");
await page.waitForURL(/#\/h\/mac\/t\//, { timeout: 30000 });
await shots("8-landed");

step("the machine, its threads by folder");
await page.goto(`${BASE}/#/h/mac`);
await page.locator(".folder").nth(1).waitFor({ timeout: 30000 });
await shots("9-host");
await page.getByRole("button", { name: "Unpair…" }).click();
await page.getByRole("button", { name: "Keep it" }).waitFor();
await shots("10-unpair-confirm");
await page.getByRole("button", { name: "Keep it" }).click();

step("home with threads");
await page.goto(`${BASE}/#/`);
await page.getByRole("heading", { name: "mac" }).waitFor({ timeout: 30000 });
await page.waitForTimeout(1500);
await shots("11-home");

step("offline new thread");
await page.goto(`${BASE}/#/h/nowhere/new`);
await page.getByText("isn't one of your machines").waitFor({ timeout: 20000 });
await shots("12-new-unknown");

await browser.close();
const real = errors.filter((e) => !/ui\.watch|favicon|401|403/.test(e));
if (real.length) console.log("console errors:\n" + real.join("\n"));
console.log("ok: every page walked");
