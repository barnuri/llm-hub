// Records a demo tour of the llm-hub UI (every page, dark then light) and writes docs/demo.gif.
// Usage: node scripts/record-demo.mjs [hub-url]
// Requires: playwright (dev dependency), ffmpeg and trash on PATH.
import { chromium } from "playwright";
import { execFileSync } from "node:child_process";
import { mkdirSync, readdirSync } from "node:fs";
import { join } from "node:path";

const HUB_URL = (process.argv[2] || "http://127.0.0.1:8888").replace(/\/$/, "");
const VIDEO_DIR = join(".playwright-mcp", "demo-video");
const GIF = "docs/demo.gif";
const SIZE = { width: 1280, height: 760 };
const DARK_PAGES = ["overview", "insights", "errors", "stats", "models", "auto", "profiles", "keys", "setup"];
const LIGHT_PAGES = ["overview", "models", "profiles"];
const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// Hides internal hostnames and e-mail addresses that the real data shows on some pages.
const REDACT = `(() => {
  const patterns = [/[\\w.-]*checkpoint\\.com[\\w./:-]*/gi, /[\\w.-]*\\.iaas\\.[\\w./:-]*/gi, /[\\w.+-]+@[\\w-]+\\.[\\w.]+/g];
  const scrub = (node) => {
    const walker = document.createTreeWalker(node, NodeFilter.SHOW_TEXT);
    for (let text = walker.nextNode(); text; text = walker.nextNode()) {
      let value = text.nodeValue;
      for (const pattern of patterns) value = value.replace(pattern, "internal.example");
      if (value !== text.nodeValue) text.nodeValue = value;
    }
  };
  new MutationObserver(() => scrub(document.body)).observe(document, { subtree: true, childList: true, characterData: true });
})()`;

if (readdirSync(".playwright-mcp", { withFileTypes: true }).some((entry) => entry.name === "demo-video")) {
  execFileSync("trash", [VIDEO_DIR]);
}
mkdirSync(VIDEO_DIR, { recursive: true });

const browser = await chromium.launch({ headless: false });
const context = await browser.newContext({ viewport: SIZE, recordVideo: { dir: VIDEO_DIR, size: SIZE }, colorScheme: "dark" });
await context.addInitScript(`try { if (!localStorage.getItem("llm-hub-theme")) localStorage.setItem("llm-hub-theme", "dark"); } catch {}`);
await context.addInitScript(REDACT);
const page = await context.newPage();

const visit = async (id, ms = 1800) => {
  await page.goto(`${HUB_URL}/${id}`);
  await page.waitForLoadState("networkidle").catch(() => undefined);
  await pause(ms);
};

for (const id of DARK_PAGES) {
  await visit(id);
  if (id === "models") {
    const search = page.getByPlaceholder("Filter models…");
    if (await search.count()) {
      await search.fill("claude");
      await pause(1200);
      await search.fill("");
      await pause(500);
    }
  }
}

await page.getByText("Light", { exact: true }).first().click();
await pause(1200);
for (const id of LIGHT_PAGES) await visit(id, 1500);

await context.close();
await browser.close();

const webm = readdirSync(VIDEO_DIR).find((name) => name.endsWith(".webm"));
if (!webm) throw new Error("no video produced");
execFileSync("ffmpeg", ["-y", "-loglevel", "error", "-i", join(VIDEO_DIR, webm), "-vf",
  "fps=8,scale=1024:-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer", GIF], { stdio: "inherit" });
execFileSync("trash", [VIDEO_DIR]);
console.log(`wrote ${GIF}`);
