// SC-32: tests/ui_review_approval.rs's browser. Pair, start a thread, and look at
// the approval card and the composer as a user would, at desktop and phone size:
// pending, a queued follow-up, the "Deny and send this" offer, then allow by `y`
// and see the card collapse to its record and the turn end. Screenshots to $SHOTS.
import { createRequire } from "node:module";
const require = createRequire(process.env.NODE_PATH + "/");
const { chromium } = require("playwright");

const { BASE, LINK, CODE, TOKEN, SHOTS } = process.env;
const DESK = { width: 1280, height: 800 };
const PHONE = { width: 390, height: 844 };
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: DESK });
await context.addCookies([{ name: "access_token", value: TOKEN, domain: "127.0.0.1", path: "/" }]);
const page = await context.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
const shot = (name) => page.screenshot({ path: `${SHOTS}/${name}.png`, fullPage: false });
const both = async (name) => {
  await page.setViewportSize(DESK); await page.waitForTimeout(250); await shot(`${name}-desktop`);
  await page.setViewportSize(PHONE); await page.waitForTimeout(250);
  const wide0 = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  if (wide0) console.log(`note: ${name}: the page is wider than a phone before sc-top is clipped`);
  await clipTop(); await page.waitForTimeout(100); await shot(`${name}-phone`);
  const wide = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  if (wide) {
    const over = await page.evaluate(() => [...document.querySelectorAll("body *")]
      .filter((e) => e.getBoundingClientRect().right > window.innerWidth + 1 && e.closest("[data-surface]") === e)
      .map((e) => e.getAttribute("data-surface") + " " + Math.round(e.getBoundingClientRect().right)));
    console.log(`warn: ${name} scrolls sideways at phone width; wider than the screen: ${over.join(", ") || "(no surface)"}`);
  }
  await page.setViewportSize(DESK);
};
const step = (s) => console.log("step:", s);
process.on("uncaughtException", async (e) => {
  console.log("FAILED:", e.message);
  try { await shot("failed"); console.log("main:", await page.locator("main").textContent()); } catch {}
  console.log("console:", errors.join("\n"));
  process.exit(1);
});

// sc-top (SC-30's, not under review here) is wider than a phone in this worktree and
// widens the whole page; clip it so the phone shots show this row's components at
// the phone's real width. The overflow check below still names it.
const clipTop = () => page.addStyleTag({ content: ".sc-top { max-width: 100vw; overflow: hidden; }" });

step("pair and start a thread");
await page.goto(LINK);
await page.getByText("Does your terminal show this code?").waitFor({ timeout: 20000 });
await page.getByRole("button", { name: "Pair", exact: true }).click();
await page.getByText("is paired").waitFor({ timeout: 20000 });
await page.goto(`${BASE}/#/h/mac/new`);
await page.getByLabel("Folder path").waitFor({ timeout: 30000 });
await page.getByLabel("Folder path").fill("/tmp");
await page.getByPlaceholder("What should Claude do?").fill("say hello");
await page.getByRole("button", { name: "Start thread" }).click();
await page.waitForURL(/#\/h\/mac\/t\//, { timeout: 30000 });

step("the approval card, pending");
const card = page.locator("[data-surface=sc-approval]");
await page.getByRole("button", { name: /Allow once/ }).waitFor({ timeout: 60000 });
const head = await card.locator("h3").textContent();
if (!/Run a command/.test(head)) throw new Error("the card's head says " + head);
if (!(await card.textContent()).includes("sh effect.sh")) throw new Error("the card doesn't show the command");
await page.locator("[data-surface=sc-composer] .strip").getByText("Waiting for your answer").waitFor({ timeout: 10000 });
await both("a-approval");

step("typing while it waits offers Deny and send this");
const field = page.getByRole("textbox", { name: "Queue a follow-up" });
await field.fill("use the other script instead");
await page.getByRole("button", { name: "Deny and send this" }).waitFor({ timeout: 5000 });
await both("b-deny-offer");

step("Enter queues it while Claude works");
await field.press("Enter");
const queued = page.locator("[data-surface=sc-composer] .queued");
await queued.first().waitFor({ timeout: 5000 });
if (!(await queued.first().textContent()).includes("use the other script instead")) throw new Error("the queued card lost the text");
await queued.first().getByRole("button", { name: "Edit" }).click();
await queued.first().locator("textarea").fill("then say goodbye");
await queued.first().getByRole("button", { name: "Done" }).click();
if (!(await queued.first().textContent()).includes("then say goodbye")) throw new Error("the edit didn't take");
await both("c-queued");

step("remove it (the replayed Claude has no second turn), then allow by y");
await queued.first().getByRole("button", { name: "Remove" }).click();
await queued.first().waitFor({ state: "detached", timeout: 5000 });
await page.locator("main").click({ position: { x: 5, y: 5 } });
await page.keyboard.press("y");
await page.locator(".record[data-surface=sc-approval]").waitFor({ timeout: 30000 });
await page.getByText("Allowed once").waitFor({ timeout: 30000 });
await page.locator(".pill[data-status=idle]").first().waitFor({ timeout: 60000 });
await page.getByRole("button", { name: "Stop" }).waitFor({ state: "detached", timeout: 10000 });
await both("d-answered");

const real = errors.filter((e) => !/ui\.watch|favicon|401|403/.test(e));
await browser.close();
if (real.length) throw new Error("console errors: " + real.join("\n"));
console.log("ok: approval, queue, deny offer, allow by key, run ended");
