// SUPERCONDUCTOR-11: tests/ui_e2e.rs's browser. Every step through the bundle's
// pages: pair by the host's link, open the host, start a thread in a folder,
// answer the approval card, and see the turn end. Screenshots go to $SHOTS.
import { createRequire } from "node:module";
const require = createRequire(process.env.NODE_PATH + "/");
const { chromium } = require("playwright");

const { BASE, LINK, CODE, TOKEN, SHOTS } = process.env;
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: { width: 1280, height: 860 } });
await context.addCookies([{ name: "access_token", value: TOKEN, domain: "127.0.0.1", path: "/" }]);
const page = await context.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
const shot = (name) => page.screenshot({ path: `${SHOTS}/${name}.png`, fullPage: false });
const step = (s) => console.log("step:", s);

step("pair by the link");
await page.goto(LINK);
await page.getByText("Does your terminal show this code?").waitFor({ timeout: 20000 });
const shown = await page.locator(".code").textContent();
if (shown.trim() !== CODE.trim()) throw new Error(`the page shows ${shown}, the host ${CODE}`);
await shot("1-pair");
await page.getByRole("button", { name: "Pair" }).click();
await page.getByText("is yours").waitFor({ timeout: 20000 });
await shot("2-paired");

step("the host's page, online");
await page.goto(`${BASE}/#/h/mac`);
await page.getByRole("button", { name: "Start a thread" }).waitFor({ timeout: 30000 });
await page.getByPlaceholder("/Users/you/project").fill("/tmp");
await page.getByPlaceholder(/What should Claude do/).fill("say hello");
await shot("3-host");
await page.getByRole("button", { name: "Start a thread" }).click();
await page.waitForURL(/#\/h\/mac\/t\//, { timeout: 30000 });

process.on("uncaughtException", async (e) => {
  console.log("FAILED:", e.message);
  try { await shot("failed"); console.log("log:", await page.locator("main").textContent()); } catch {}
  console.log("console:", errors.join("\n"));
  process.exit(1);
});
step("the transcript, and the approval card");
await page.locator(".user").first().waitFor({ timeout: 30000 });
await page.getByRole("button", { name: "Allow once" }).waitFor({ timeout: 60000 });
await shot("4-approval");
await page.getByRole("button", { name: "Allow once" }).click();
await page.getByRole("button", { name: "Allow once" }).waitFor({ state: "detached", timeout: 60000 });
await page.locator(".pill[data-status=idle]").first().waitFor({ timeout: 60000 });
await page.waitForFunction(() => document.querySelectorAll(".agent, .summary, .tool").length > 0, null, { timeout: 30000 });
await shot("5-done");

step("the sidebar lists mac and the thread");
const side = await page.locator(".sc-nav").textContent();
if (!side.includes("mac")) throw new Error("the sidebar has no mac: " + side);

step("signed out, the home page offers GitHub");
const anon = await (await browser.newContext()).newPage();
await anon.goto(`${BASE}/#/`);
await anon.getByText("Sign in with GitHub").first().waitFor({ timeout: 20000 });
await browser.close();
const real = errors.filter((e) => !/ui\.watch|favicon|401|403/.test(e));
if (real.length) throw new Error("console errors: " + real.join("\n"));
console.log("ok: every step through the bundle");
