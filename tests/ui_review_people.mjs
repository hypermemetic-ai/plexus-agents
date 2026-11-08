// tests/ui_review_people.rs's browser: Chromium, a laptop (1280×800) and a phone
// (390×844, touch), each holding the admin's own browser session cookie.
// Laptop: account menu → Act as… → create carol (user@*) → the band shows, the
// "Act as…" entry and the people page are gone → Return → back as the admin.
// Phone: the people page → act as bob → the band → Return.
// Screenshots to $SHOTS: people-*.png. Exit 1 on a failure.
import { createRequire } from "node:module";
const require = createRequire(process.env.NODE_PATH + "/");
const pw = require("playwright");

const { BASE, COOKIE, DESK, PHONE, SHOTS } = process.env;
const browser = await pw.chromium.launch();
const fails = [];
const ok = (cond, what) => { if (!cond) fails.push(what); console.log(`${cond ? "ok  " : "FAIL"} ${what}`); };
const errors = [];

async function open(viewport, id, phone) {
  const opts = phone ? { ...pw.devices["iPhone 13"], viewport, screen: viewport } : { viewport };
  delete opts.defaultBrowserType;
  const ctx = await browser.newContext(opts);
  await ctx.addCookies([{ name: COOKIE, value: id, domain: "127.0.0.1", path: "/", httpOnly: true, sameSite: "Lax" }]);
  const page = await ctx.newPage();
  page.on("pageerror", (e) => errors.push(String(e)));
  return page;
}
const band = (page) => page.locator("[data-surface=sc-acting]");
const sideways = (page, tag) => page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth)
  .then((w) => ok(w <= 1, `${tag}: nothing scrolls sideways (+${w})`));

try {
  // ── laptop ──
  const d = await open({ width: 1280, height: 800 }, DESK, false);
  await d.goto(`${BASE}/#/`);
  await d.locator(".sc-top .me").waitFor({ timeout: 20000 });
  await d.locator(".sc-top .me").click();
  const actItem = d.locator(".menu .item", { hasText: "Act as…" });
  await actItem.waitFor({ timeout: 10000 });
  ok(await actItem.isVisible(), "laptop: the admin's account menu offers Act as…");
  await d.screenshot({ path: `${SHOTS}/people-desktop-menu.png` });
  await actItem.click();
  await d.locator("[data-surface=sc-people] .person").first().waitFor({ timeout: 10000 });
  const rows = await d.locator("[data-surface=sc-people] .person").count();
  ok(rows >= 2, `laptop: the people page lists who this service knows (${rows})`);
  ok(await d.locator(".person", { hasText: "Admins can't be acted as" }).count() === 0, "laptop: the one admin listed is you");
  await d.screenshot({ path: `${SHOTS}/people-desktop.png` });
  await d.locator("[data-surface=sc-people] input[aria-label=Name]").fill("carol");
  await d.getByRole("button", { name: "Create and act as" }).click();
  await band(d).waitFor({ timeout: 20000 });
  const said = await band(d).innerText();
  ok(/Acting as\s+carol/.test(said) && /Ada/.test(said), `laptop: the band says whom and for whom (${said.replace(/\s+/g, " ")})`);
  await d.waitForTimeout(400);
  await d.screenshot({ path: `${SHOTS}/people-desktop-acting.png` });
  await d.locator(".sc-top .me").click();
  await d.locator(".menu").waitFor();
  ok(await d.locator(".menu .item", { hasText: "Act as…" }).count() === 0, "laptop: acting, the Act as… entry is gone");
  await d.screenshot({ path: `${SHOTS}/people-desktop-acting-menu.png` });
  await d.keyboard.press("Escape");
  await d.goto(`${BASE}/#/people`);
  await d.locator("[data-surface=sc-people] .sc-empty").waitFor({ timeout: 10000 });
  ok(await d.locator("[data-surface=sc-people] .person").count() === 0, "laptop: acting, the people page offers nothing");
  await d.screenshot({ path: `${SHOTS}/people-desktop-acting-people.png` });
  await band(d).locator(".return").click();
  await d.locator("[data-surface=sc-people] .person").first().waitFor({ timeout: 20000 });
  ok(await band(d).count() === 0, "laptop: returned, the band is gone");
  ok(await d.locator(".person", { hasText: "carol" }).locator(".tag", { hasText: "Test user" }).count() === 1, "laptop: carol is listed, a test user");
  await d.screenshot({ path: `${SHOTS}/people-desktop-returned.png` });

  // ── phone ──
  const m = await open({ width: 390, height: 844 }, PHONE, true);
  await m.goto(`${BASE}/#/people`);
  await m.locator("[data-surface=sc-people] .person").first().waitFor({ timeout: 20000 });
  await sideways(m, "phone, people");
  await m.screenshot({ path: `${SHOTS}/people-mobile.png` });
  await m.screenshot({ path: `${SHOTS}/people-mobile-full.png`, fullPage: true });
  const bob = m.locator(".person").filter({ hasText: "user_01BOB" });
  await bob.getByRole("button", { name: "Act as" }).click();
  await band(m).waitFor({ timeout: 20000 });
  ok(/Acting as/.test(await band(m).innerText()), "phone: the band shows");
  await sideways(m, "phone, acting");
  const tap = await band(m).locator(".return").boundingBox();
  ok(tap && tap.height >= 36, `phone: Return is a thumb's target (${tap && Math.round(tap.height)}px)`);
  await m.waitForTimeout(400);
  await m.screenshot({ path: `${SHOTS}/people-mobile-acting.png` });
  await band(m).locator(".return").click();
  await m.locator("[data-surface=sc-people] .person").first().waitFor({ timeout: 20000 });
  ok(await band(m).count() === 0, "phone: returned");
} catch (e) {
  fails.push(String(e.message || e));
  console.log("FAILED:", e.message);
}
if (errors.length) console.log("page errors:", errors.join("\n"));
await browser.close();
process.exit(fails.length ? 1 : 0);
