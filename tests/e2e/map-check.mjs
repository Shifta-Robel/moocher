// The work map and the manager panel in headless Chromium. Usage:
//   node map-check.mjs <hub url> <user> <password> <shot dir>
// Needs PW_DIR (a directory with node_modules/playwright). PW_CHROMIUM names another Chromium to use.
const pw = await import(`${process.env.PW_DIR}/node_modules/playwright/index.mjs`);
const { chromium } = pw.default ?? pw;
const [hub, user, password, shots] = process.argv.slice(2);
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? "ok" : "FAIL"} ${what}`); if (!ok) failed++; };
const browser = await chromium.launch(process.env.PW_CHROMIUM ? { executablePath: process.env.PW_CHROMIUM, args: ["--no-sandbox", "--disable-gpu", "--single-process", "--no-zygote"] } : {});
const ctx = await browser.newContext({ viewport: { width: 1280, height: 860 } });
const page = await ctx.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
const step = async (what, f) => {
  try { await f(); check(true, what); }
  catch (e) { check(false, `${what}: ${String(e.message).split("\n")[0]}`); await page.screenshot({ path: `${shots}/map-failed.png` }).catch(() => {}); }
};
const noSideScroll = () => page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth);

await page.goto(hub + "/login");
await page.getByLabel("User").fill(user);
await page.getByLabel("Password").fill(password);
await page.getByRole("button", { name: /log in|sign in/i }).click();
await page.waitForURL((u) => !u.pathname.startsWith("/login"));

await step("the nav leads to the work map", async () => {
  await page.getByRole("link", { name: "Work map" }).first().click();
  await page.waitForURL(/\/work$/);
  await page.getByRole("heading", { name: "Work map" }).waitFor();
});
await step("a task's agents are drawn with lines between them", async () => {
  await page.locator("[data-session]").nth(2).waitFor({ timeout: 90000 });
  const n = await page.locator("section svg path").count();
  if (n < 2) throw new Error(`${n} lines`);
});
await step("cards show node, model and start time", async () => {
  const t = await page.locator("[data-session]").first().innerText();
  for (const w of ["hub", "mock/map"]) if (!t.includes(w)) throw new Error(`no ${w} in: ${t}`);
  if (!/\d:\d\d/.test(t)) throw new Error(`no time in: ${t}`);
});
await step("children sit to the right of their parent", async () => {
  const boxes = await page.locator("[data-session]").evaluateAll((els) => els.map((e) => e.getBoundingClientRect().left));
  if (new Set(boxes).size < 2) throw new Error(`one column only: ${boxes}`);
});
await page.screenshot({ path: `${shots}/work-desktop.png`, fullPage: true });
await step("a card opens its session", async () => {
  await page.locator("[data-session]").first().click();
  await page.waitForURL(/\/instances\//);
  await page.goBack();
});
await step("the manager answers", async () => {
  await page.getByRole("button", { name: "ask the manager" }).click();
  await page.getByLabel("ask the manager", { exact: true }).last().fill("what is going on");
  await page.keyboard.press("Enter");
  await page.getByText("Three agents are working.").waitFor({ timeout: 30000 });
  await page.screenshot({ path: `${shots}/manager.png` });
});
await step("asking to go somewhere goes there, with the panel still open", async () => {
  await page.getByLabel("ask the manager", { exact: true }).last().fill("take me to the quotas");
  await page.keyboard.press("Enter");
  await page.waitForURL(/\/quotas$/, { timeout: 30000 });
  await page.getByRole("region", { name: "manager" }).waitFor();
});
await step("a link it offers works", async () => {
  await page.getByLabel("ask the manager", { exact: true }).last().fill("offer the map");
  await page.keyboard.press("Enter");
  await page.getByRole("region", { name: "manager" }).getByRole("link", { name: "Work map" }).click();
  await page.waitForURL(/\/work$/);
});
await step("text from the model is not rendered as HTML", async () => {
  await page.getByLabel("ask the manager", { exact: true }).last().fill("markup please");
  await page.keyboard.press("Enter");
  await page.getByText("<img src=x onerror=alert(1)>").waitFor({ timeout: 30000 });
  if (await page.locator("section img[src='x']").count()) throw new Error("rendered as an image");
});
await step("clearing forgets the conversation", async () => {
  await page.getByRole("button", { name: "Clear" }).click();
  if (await page.getByText("Three agents are working.").count()) throw new Error("still there");
});
await step("dark mode works", async () => {
  await page.evaluate(() => document.documentElement.classList.add("dark"));
  await page.screenshot({ path: `${shots}/work-dark.png`, fullPage: true });
  await page.evaluate(() => document.documentElement.classList.remove("dark"));
});
await step("on a phone the page itself does not scroll sideways", async () => {
  await page.setViewportSize({ width: 390, height: 800 });
  await page.waitForTimeout(300);
  if (!(await noSideScroll())) throw new Error("it does");
  await page.screenshot({ path: `${shots}/work-phone.png` });
});
await step("on a phone the panel fits and the graph scrolls inside its card", async () => {
  const b = await page.getByRole("region", { name: "manager" }).boundingBox();
  if (b.x < 0 || b.x + b.width > 390) throw new Error(`panel at ${b.x}+${b.width}`);
  await page.getByRole("button", { name: "close the manager" }).click();
  const r = await page.locator("section .overflow-x-auto").first().evaluate((e) => e.scrollWidth > e.clientWidth);
  if (!r) throw new Error("no scroll area");
  await page.screenshot({ path: `${shots}/work-phone-closed.png` });
});
check(errors.length === 0, `no page errors ${errors.join("; ").slice(0, 300)}`);
await browser.close();
process.exit(failed ? 1 : 0);
