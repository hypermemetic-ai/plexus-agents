// superconductor-ui/modes: tests/ui_review_modes.rs's browser. Pairs `mac`
// (--allow-bypass) and `box` (without), then walks the permission-mode UI as a
// person would, checking as it goes, and screenshots each screen at desktop and
// phone size into $SHOTS (modes-N-NAME-desktop.png, -phone.png). Exit 1 on a
// failure.
import { createRequire } from "node:module";
const require = createRequire(process.env.NODE_PATH + "/");
const { chromium } = require("playwright");

const { BASE, LINK_MAC, LINK_BOX, TOKEN, SHOTS, ROOT } = process.env;
const DESKTOP = { width: 1280, height: 800 };
const PHONE = { width: 390, height: 844 };
const browser = await chromium.launch();
const context = await browser.newContext({ viewport: DESKTOP });
await context.addCookies([{ name: "access_token", value: TOKEN, domain: "127.0.0.1", path: "/" }]);
const page = await context.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => { if (m.type() === "error") errors.push(m.text()); });
const fails = [];
const ok = (cond, what) => { if (!cond) fails.push(what); console.log(`${cond ? "ok  " : "FAIL"} ${what}`); };
const step = (s) => console.log("step:", s);
process.on("uncaughtException", async (e) => {
  console.log("FAILED:", e.message);
  try { await page.screenshot({ path: `${SHOTS}/modes-failed.png` }); console.log("text:", await page.locator("body").textContent()); } catch {}
  console.log("console:", errors.join("\n"));
  process.exit(1);
});
async function shots(name, show) {
  await page.waitForTimeout(300);
  if (show) await page.locator(show).first().scrollIntoViewIfNeeded();
  await page.screenshot({ path: `${SHOTS}/${name}-desktop.png` });
  await page.setViewportSize(PHONE);
  await page.waitForTimeout(300);
  if (show) await page.locator(show).first().evaluate((el) => el.scrollIntoView({ block: "center" }));
  await page.waitForTimeout(100);
  const wide = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  ok(wide <= 0, `${name}: nothing scrolls sideways on a phone (${wide}px)`);
  await page.screenshot({ path: `${SHOTS}/${name}-phone.png`, fullPage: true });
  await page.setViewportSize(DESKTOP);
  await page.waitForTimeout(150);
}
async function pair(link) {
  await page.goto(link);
  await page.getByText("Does your terminal show this code?").waitFor({ timeout: 20000 });
  await page.getByRole("button", { name: "Pair", exact: true }).click();
  await page.getByText("is paired").waitFor({ timeout: 20000 });
}
const folderRow = (name) => page.getByRole("radiogroup", { name: "Folder" }).getByRole("radio", { name: new RegExp(`^${name}`) });
const bypassRadio = () => page.getByRole("radio", { name: /Bypass all permissions/ });

step("pair mac and box");
await pair(LINK_MAC);
await pair(LINK_BOX);

step("mac's page: its roots, bypass allowed");
await page.goto(`${BASE}/#/h/mac`);
await page.getByText("What mac allows").waitFor({ timeout: 30000 });
ok(await page.locator(".on-bypass").isVisible(), "mac says bypass is allowed");
await shots("modes-1-host-mac");

step("box's page: bypass off, with the flag to turn it on");
await page.goto(`${BASE}/#/h/box`);
await page.getByText("What box allows").waitFor({ timeout: 30000 });
ok(await page.getByText("To allow it, restart the host with").isVisible(), "box says how to allow bypass");
await shots("modes-2-host-box");

step("a new thread on box: the folders, bypass disabled");
await page.goto(`${BASE}/#/h/box/new`);
await folderRow("superconductor").waitFor({ timeout: 30000 });
const names = await page.getByRole("radiogroup", { name: "Folder" }).getByRole("radio").allTextContents();
ok(["superconductor", "plexus-ui", "hyperforge", "notes"].every((n) => names.some((t) => t.startsWith(n))), `the projects under the root are listed: ${names.join(" | ")}`);
ok(!names.some((t) => t.includes(".hidden")), "a hidden folder isn't");
ok(await page.locator(".row .git").count() === 3, "the three git checkouts are marked");
ok(await bypassRadio().isDisabled(), "bypass is disabled on a host without --allow-bypass");
ok(await page.getByText(/Off on box. To allow it, restart the host there with --allow.bypass/).isVisible(), "and says how to turn it on");
ok(await page.getByRole("radio", { name: /Ask before each action/ }).getAttribute("aria-checked") === "true", "asking is the default");
await page.getByPlaceholder("What should Claude do?").fill("Find why the folder list flickers on reconnect.");
await shots("modes-3-new-box", ".sc-modes");
await page.getByLabel("Folder path").fill("plex");
await page.waitForTimeout(100);
const filtered = await page.getByRole("radiogroup", { name: "Folder" }).getByRole("radio").allTextContents();
ok(filtered.length === 2 && filtered[0].startsWith("plexus-ui"), `typing filters the list: ${filtered.join(" | ")}`);
await shots("modes-4-filter");

step("a new thread on mac: bypass behind a confirm");
await page.goto(`${BASE}/#/h/mac/new`);
await folderRow("superconductor").waitFor({ timeout: 30000 });
await page.getByPlaceholder("What should Claude do?").fill("Run the effect script and tell me what it changed.");
await folderRow("superconductor").click();
ok(await page.getByLabel("Folder path").inputValue() === `${ROOT}/superconductor`, "a picked folder fills the path");
ok(!(await bypassRadio().isDisabled()), "bypass is enabled on a host with --allow-bypass");
await bypassRadio().click();
await page.getByText("Let Claude act without asking?").waitFor();
ok(await bypassRadio().getAttribute("aria-checked") === "false", "bypass isn't chosen until confirmed");
await shots("modes-5-bypass-confirm", ".confirm");
await page.getByRole("button", { name: "Keep asking" }).click();
ok(await page.getByRole("radio", { name: /Ask before each action/ }).getAttribute("aria-checked") === "true", "Keep asking keeps the mode");
await page.getByRole("button", { name: "Start thread" }).click();
await page.waitForURL(/#\/h\/mac\/t\//, { timeout: 30000 });

step("the approval: Always allow");
await page.getByRole("button", { name: /Always allow this command/ }).waitFor({ timeout: 60000 });
ok((await page.locator(".keeps").textContent()).includes("also allows running sh effect.sh without asking, in this thread from now on"), "the card says what is remembered, and for how long");
ok(await page.locator(".head .mode").textContent().then((t) => t.includes("Ask before each action")), "the header shows the mode");
await shots("modes-6-always", ".keeps");
await page.getByRole("button", { name: /Always allow this command/ }).click();
await page.getByRole("button", { name: "Allow once" }).waitFor({ state: "detached", timeout: 60000 });
await page.locator(".pill[data-status=idle]").first().waitFor({ timeout: 60000 });
await shots("modes-7-remembered");

step("the header: change the mode, then bypass");
await page.locator(".head .mode").click();
await page.getByRole("radiogroup", { name: "Permissions for this thread" }).waitFor();
await shots("modes-8-mode-panel");
await page.getByRole("radio", { name: /Plan only/ }).click();
await page.getByText("Set to plan only. It applies from Claude's next turn.").waitFor({ timeout: 30000 });
ok((await page.locator(".head .mode").textContent()).includes("Plan only"), "set_mode took: the header says Plan only");
await bypassRadio().click();
await page.getByRole("button", { name: "Bypass permissions" }).click();
await page.locator(".head .mode[data-bypass]").waitFor({ timeout: 30000 });
await shots("modes-9-bypass-set");
await page.getByRole("button", { name: "Done" }).click();
ok(await page.getByText("Permissions are bypassed").isVisible(), "a bypassed thread says so under its header");
await shots("modes-10-bypassed");

await browser.close();
const real = errors.filter((e) => !/ui\.watch|favicon|401|403/.test(e));
if (real.length) console.log("console errors:\n" + real.join("\n"));
if (fails.length) { console.log("FAILED:\n" + fails.join("\n")); process.exit(1); }
console.log("ok: the modes walk");
