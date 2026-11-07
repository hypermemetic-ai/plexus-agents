// SC-30: tests/ui_review_frame.rs's browser. Screenshots of home, a host, a
// thread (an approval pending) and the component sheet at 1280×800 and 390×844,
// light and dark, into $SHOTS; and a check that no page scrolls sideways.
import { createRequire } from "node:module";
const require = createRequire(process.env.NODE_PATH + "/");
const { chromium } = require("playwright");

const { BASE, TOKEN, THREAD, SHOTS } = process.env;
const browser = await chromium.launch();
const VIEWS = [["desktop", 1280, 800], ["phone", 390, 844]];
const PAGES = [["home", "#/"], ["host", "#/h/mac"], ["thread", `#/h/mac/t/${THREAD}`], ["sheet", "#/sheet"], ["pair", "#/pair"]];
const problems = [];
for (const scheme of ["light", "dark"]) {
  for (const [view, width, height] of VIEWS) {
    const context = await browser.newContext({ viewport: { width, height }, colorScheme: scheme, deviceScaleFactor: view === "phone" ? 2 : 1, isMobile: view === "phone", hasTouch: view === "phone" });
    await context.addCookies([{ name: "access_token", value: TOKEN, domain: "127.0.0.1", path: "/" }]);
    const page = await context.newPage();
    page.on("pageerror", (e) => problems.push(`${view}/${scheme}: ${e}`));
    page.on("console", (m) => { if (m.type() === "error" && !/ui\.watch|favicon|401|403/.test(m.text())) problems.push(`${view}/${scheme}: ${m.text()}`); });
    for (const [name, hash] of PAGES) {
      await page.goto(`${BASE}/${hash}`);
      await page.waitForTimeout(name === "thread" ? 5000 : 2500);
      const wide = await page.evaluate(() => document.documentElement.scrollWidth - innerWidth);
      if (wide > 0) problems.push(`${name} ${view}/${scheme}: scrolls sideways by ${wide}px`);
      await page.screenshot({ path: `${SHOTS}/${name}-${view}-${scheme}.png` });
      if (name === "sheet") {
        await page.evaluate(() => document.querySelectorAll(".main, .sheet, .body").forEach((e) => (e.scrollTop = e.scrollHeight)));
        await page.waitForTimeout(800);
        await page.screenshot({ path: `${SHOTS}/${name}-end-${view}-${scheme}.png` });
      }
    }
    if (view === "phone" && scheme === "light") {
      await page.goto(`${BASE}/#/h/mac/t/${THREAD}`);
      await page.waitForTimeout(1500);
      const dom = await page.evaluate(() => {
        const f = document.querySelector(".sc-frame");
        return f ? f.outerHTML.replace(/<svg[\s\S]*?<\/svg>/g, "<svg/>").slice(0, 1500) : "no .sc-frame";
      });
      console.log("dom:", dom);
    }
    await context.close();
  }
}
await browser.close();
if (problems.length) console.log("problems:\n" + problems.join("\n"));
console.log("ok: shots in", SHOTS);
