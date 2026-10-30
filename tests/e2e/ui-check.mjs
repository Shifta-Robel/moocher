// Drives the dashboard in headless Chromium. Usage:
//   node ui-check.mjs <hub url> <user> <password> <question asker session> <shot dir>
// Needs PW_DIR (a directory with node_modules/playwright).
// PW_CHROMIUM names another Chromium to use.
const pw = await import(`${process.env.PW_DIR}/node_modules/playwright/index.mjs`);
const { chromium } = pw.default ?? pw;
const [hub, user, password, asker, shots] = process.argv.slice(2);
let failed = 0;
const check = (ok, what) => { console.log(`${ok ? "ok" : "FAIL"} ${what}`); if (!ok) failed++; };
let shot = 0;
const step = async (what, f) => {
  try { await f(); check(true, what); }
  catch (e) {
    check(false, `${what}: ${String(e.message).split("\n")[0]}`);
    await page.screenshot({ path: `${shots}/failed-${++shot}.png` }).catch(() => {});
  }
};

const launch = () => chromium.launch(process.env.PW_CHROMIUM ? { executablePath: process.env.PW_CHROMIUM, args: ["--no-sandbox", "--disable-gpu", "--single-process", "--no-zygote"] } : {});
const browser = await launch();
const ctx = await browser.newContext({ viewport: { width: 1280, height: 860 } });
const page = await ctx.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => m.type() === "error" && !/401|favicon/.test(m.text()) && errors.push(m.text()));

await step("an unauthenticated visit lands on the login page", async () => {
  await page.goto(hub + "/instances");
  await page.waitForURL(/\/login\?next=\/instances/);
});
await step("a wrong password shows an error", async () => {
  await page.getByLabel("User").fill(user);
  await page.getByLabel("Password").fill("wrong-password");
  await page.getByRole("button", { name: "Log in" }).click();
  await page.getByRole("alert").waitFor();
});
await step("logging in goes to the page asked for", async () => {
  await page.getByLabel("Password").fill(password);
  await page.getByRole("button", { name: "Log in" }).click();
  await page.waitForURL(/\/instances$/);
  await page.getByRole("heading", { name: "Instances" }).waitFor();
});
await step("overview shows the hub node and the live status strip", async () => {
  await page.getByRole("link", { name: "Overview" }).first().click();
  await page.getByRole("heading", { name: "Overview" }).waitFor();
  await page.getByText("1/1 nodes").waitFor();
});
await page.screenshot({ path: `${shots}/overview-light.png`, fullPage: true });

let sid = "";
await step("Templates → New instance starts a builder and opens it", async () => {
  await page.getByRole("link", { name: "Templates" }).first().click();
  await page.locator("div", { hasText: /^builder/ }).getByRole("button", { name: "New instance" }).first().click();
  await page.getByPlaceholder("What should it do?").fill("Write greet(name) in greet.py with pytest tests.");
  await page.getByRole("button", { name: "Start" }).click();
  await page.waitForURL(/\/instances\/s-[0-9a-f]+$/);
  sid = page.url().split("/").pop();
});
await step("the instance page updates live until the turn is done (todos 3/3)", async () => {
  await page.getByText("idle", { exact: true }).waitFor({ timeout: 120000 });
  await page.getByRole("tab", { name: "Todos" }).click();
  await page.getByText("run tests").waitFor();
  const done = await page.locator("li.line-through").count();
  if (done < 3) throw new Error(`${done} completed todos shown`);
});
await step("the preamble tab shows what it was told", async () => {
  await page.getByRole("tab", { name: "What it was told" }).click();
  await page.getByText(`You are instance \`${sid}\``, { exact: false }).waitFor();
});
await step("sending a message queues it", async () => {
  await page.getByLabel("message").fill("Add a docstring to greet.");
  await page.getByRole("button", { name: "Send" }).click();
  await page.getByText("Queued for its next step").waitFor();
});
await step("Open in OpenCode opens this instance's conversation in OpenCode's web app", async () => {
  const [popup] = await Promise.all([ctx.waitForEvent("page"), page.getByRole("button", { name: "Open in OpenCode" }).click()]);
  await popup.waitForURL(/:7444\/[A-Za-z0-9_-]+\/session\/ses_/, { timeout: 20000 });
  await popup.waitForLoadState("domcontentloaded");
  const html = await popup.content();
  if (!/\/assets\//.test(html)) throw new Error("no OpenCode app in the popup");
  await popup.getByText("Write greet(name) in greet.py", { exact: false }).first().waitFor({ timeout: 30000 });
  await popup.screenshot({ path: `${shots}/opencode-attached.png` });
  await popup.close();
});

await step("a question from an agent appears in the inbox and is answered by a click", async () => {
  await page.getByRole("link", { name: /Questions/ }).first().click();
  const card = page.locator("div", { hasText: "Which port should the service listen on?" }).last();
  await card.waitFor({ timeout: 60000 });
  await page.getByText("blocks 1").waitFor({ timeout: 5000 });
  await page.screenshot({ path: `${shots}/questions.png`, fullPage: true });
  await page.getByRole("button", { name: "9090" }).click();
  await page.getByText("Answer delivered").waitFor();
  await page.getByText("No questions waiting").waitFor({ timeout: 15000 });
});
await step("the question mode can be switched", async () => {
  await page.getByRole("radio", { name: /Vital only/ }).click();
  await page.getByText("Question mode: vital only").waitFor();
  await page.getByRole("radio", { name: /Important and vital/ }).click();
});

await step("tasks: add two, reorder with the arrows", async () => {
  await page.getByRole("link", { name: /Tasks/ }).first().click();
  for (const t of ["first task", "second task"]) {
    await page.getByRole("button", { name: "Add task" }).click();
    await page.getByLabel("Title").fill(t);
    await page.getByLabel("When").selectOption("on_demand");
    await page.getByRole("button", { name: "Add", exact: true }).click();
    await page.getByText(t).first().waitFor();
  }
  const col = page.locator("section", { has: page.getByRole("heading", { name: "On demand" }) });
  await col.getByText("second task").locator("xpath=ancestor::div[@draggable]").getByLabel("move up").click();
  await page.waitForFunction(() => {
    const s = [...document.querySelectorAll("section")].find((x) => x.querySelector("h2")?.textContent === "On demand");
    const t = [...(s?.querySelectorAll("[draggable] [title]") ?? [])].map((e) => e.textContent);
    return t.findIndex((x) => x.includes("second")) < t.findIndex((x) => x.includes("first"));
  });
});

await step("nodes: Add machine shows a join command", async () => {
  await page.getByRole("link", { name: "Nodes & proxies" }).first().click();
  await page.getByRole("button", { name: "Add machine" }).click();
  await page.getByLabel("Name").fill("vps-test");
  await page.getByRole("button", { name: "Create join command" }).click();
  await page.getByText(/moocher-node join .* --token moojoin\./).waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("dialog").waitFor({ state: "detached", timeout: 5000 });
});

await step("dark mode via the theme toggle", async () => {
  const btn = page.getByRole("button", { name: /theme:/ }).first();
  for (let i = 0; i < 3 && !(await page.evaluate(() => document.documentElement.classList.contains("dark"))); i++) await btn.click();
  if (!(await page.evaluate(() => document.documentElement.classList.contains("dark")))) throw new Error("no dark class");
  await page.goto(hub + "/");
  if (!(await page.evaluate(() => document.documentElement.classList.contains("dark")))) throw new Error("not remembered");
  await page.screenshot({ path: `${shots}/overview-dark.png`, fullPage: true });
});

await step("phone layout: bottom tab bar, no sidebar, activity rows expand, no page scrolls sideways", async () => {
  // Its own browser: a single-process Chromium cannot hold a second context.
  const phoneBrowser = await launch();
  const phone = await phoneBrowser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true, storageState: await ctx.storageState() });
  const p = await phone.newPage();
  await p.goto(hub + "/instances/" + sid);
  const tabs = p.locator("nav[aria-label=main]").last();
  await tabs.getByText("Questions").waitFor();
  if (await p.locator("aside").isVisible()) throw new Error("sidebar visible on a phone");
  const overflow = await p.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
  if (overflow > 1) throw new Error(`page scrolls sideways by ${overflow}px`);
  await p.screenshot({ path: `${shots}/instance-phone.png`, fullPage: true });
  await p.getByRole("tab", { name: "Activity" }).click();
  await p.locator("li > button[aria-expanded=false]:not([disabled])").first().click();
  await p.locator("li > pre").first().waitFor();
  await p.screenshot({ path: `${shots}/activity-phone.png`, fullPage: true });
  const wide = [];
  for (const r of ["/", "/instances", "/questions", "/tasks", "/quotas", "/templates", "/nodes", "/artifacts", "/notifications", "/audit", "/settings"]) {
    await p.goto(hub + r);
    await p.locator("main h1").first().waitFor();
    await p.waitForTimeout(800);
    const o = await p.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    if (o > 1) wide.push(`${r} by ${o}px`);
  }
  if (wide.length) throw new Error(`pages scroll sideways: ${wide.join(", ")}`);
  await phoneBrowser.close();
});

await step("logout returns to the login page", async () => {
  await page.getByRole("button", { name: "log out" }).first().click();
  await page.waitForURL(/\/login/);
});
check(errors.length === 0, `no JavaScript errors (${errors.slice(0, 3).join(" | ")})`);
await browser.close();
process.exit(failed ? 1 : 0);
