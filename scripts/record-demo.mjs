// Records a demo walkthrough of the llm-hub UI and produces docs/demo.gif.
// Usage: node scripts/record-demo.mjs [hub-url]
// Requires: `npm i -D playwright` (or a global install) and ffmpeg on PATH.
import { chromium } from "playwright";
import { execSync } from "node:child_process";
import { mkdirSync, readdirSync, renameSync, rmSync } from "node:fs";
import { join } from "node:path";

const HUB_URL = process.argv[2] || "http://127.0.0.1:8410";
const OUT_DIR = ".playwright-mcp";
const VIDEO_DIR = join(OUT_DIR, "demo-video");
const GIF = "docs/demo.gif";
const SIZE = { width: 1180, height: 680 };
const pause = (ms) => new Promise((r) => setTimeout(r, ms));

mkdirSync(VIDEO_DIR, { recursive: true });
mkdirSync("docs", { recursive: true });

const browser = await chromium.launch({ headless: false });
const context = await browser.newContext({
  viewport: SIZE,
  recordVideo: { dir: VIDEO_DIR, size: SIZE },
});
const page = await context.newPage();

await page.goto(HUB_URL);
await pause(1500);

// Models: filter, then copy one id
const search = page.getByPlaceholder("Filter models…");
await search.fill("llama");
await pause(1200);
await search.fill("");
await pause(600);
const copyButtons = page.getByRole("button", { name: "Copy", exact: true });
if (await copyButtons.count()) { await copyButtons.first().click(); await pause(900); }

// Stats
await page.getByRole("tab", { name: "Stats" }).click();
await pause(1500);

// Usage
await page.getByRole("tab", { name: "Usage" }).click();
await pause(1500);

// Profiles: open the add form, then cancel
await page.getByRole("tab", { name: "Profiles" }).click();
await pause(1000);
await page.getByRole("button", { name: "Add profile" }).click();
await pause(1400);
await page.getByRole("button", { name: "Cancel" }).click();
await pause(600);

// Keys
await page.getByRole("tab", { name: "API keys" }).click();
await pause(1200);

// Setup: flip through targets
await page.getByRole("tab", { name: "Setup" }).click();
await pause(800);
for (const target of ["openai-python", "langchain", "claude-code"]) {
  await page.selectOption("#setup-target", target);
  await pause(1100);
}

await context.close();
await browser.close();

const webm = readdirSync(VIDEO_DIR).find((f) => f.endsWith(".webm"));
if (!webm) throw new Error("no video produced");
const source = join(VIDEO_DIR, webm);
execSync(`ffmpeg -y -i "${source}" -vf "fps=8,scale=960:-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff[p];[b][p]paletteuse=dither=bayer" "${GIF}"`, { stdio: "inherit" });
rmSync(VIDEO_DIR, { recursive: true, force: true });
console.log(`wrote ${GIF}`);
