import assert from "node:assert/strict";
import { createReadStream, existsSync } from "node:fs";
import { createServer } from "node:http";
import { extname, resolve, sep } from "node:path";
import { test } from "node:test";
import { chromium } from "playwright";

const ROOT = resolve(import.meta.dirname, "..");
const UI_ROOT = resolve(ROOT, "ui");
const SCREENSHOT_DIR = resolve(ROOT, ".playwright-mcp");
const MODELS = [
  {
    id: "copilot/auto-efficiency",
    display_name: "Auto (Efficiency) · copilot",
    description: "Prioritizes cost. Use for straightforward tasks, quick edits, and simple questions.",
    auto_tier: "efficiency",
  },
  {
    id: "copilot/auto",
    display_name: "Auto (Balance) · copilot",
    description: "Balances cost, quality, and latency. Use for everyday coding and debugging.",
    auto_tier: "balance",
  },
  {
    id: "copilot/auto-intelligence",
    display_name: "Auto (Intelligence) · copilot",
    description: "Prioritizes quality. Use for complex bugs and tasks that need deeper reasoning.",
    auto_tier: "intelligence",
  },
  {
    id: "copilot/auto-fast",
    display_name: "Auto (Fast) · copilot",
    description: "Additional hub routing tier. Official Copilot guidance covers Efficiency, Balance, and Intelligence.",
    auto_tier: "fast",
  },
];

function sendJson(response, body) {
  response.writeHead(200, { "content-type": "application/json; charset=utf-8" });
  response.end(JSON.stringify(body));
}

function createUiServer() {
  return createServer((request, response) => {
    const pathname = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
    if (pathname === "/api/profiles") {
      sendJson(response, {
        profiles: [{ name: "copilot", label: "Copilot", enabled: true, healthy: true }],
        default_fallbacks: [],
        readonly: false,
        persistent: false,
        auth_enabled: false,
        version: "test",
      });
      return;
    }
    if (pathname === "/v1/models") {
      sendJson(response, { object: "list", data: MODELS });
      return;
    }
    if (pathname === "/api/copilot/auto/decisions") {
      sendJson(response, { decisions: [] });
      return;
    }
    if (pathname === "/api/errors") {
      sendJson(response, { total_errors: 0, recent: [] });
      return;
    }

    const requested = ["/", "/auto", "/models"].includes(pathname) ? "/index.html" : pathname;
    const file = resolve(UI_ROOT, `.${requested}`);
    if (!file.startsWith(`${UI_ROOT}${sep}`) || !existsSync(file)) {
      response.writeHead(404).end();
      return;
    }
    const type = extname(file) === ".js" ? "text/javascript" : extname(file) === ".css" ? "text/css" : "text/html";
    response.writeHead(200, { "content-type": `${type}; charset=utf-8` });
    createReadStream(file).pipe(response);
  });
}

test("Copilot Auto descriptions render once per tier", async () => {
  const server = createUiServer();
  await new Promise((resolveListen, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolveListen);
  });
  const address = server.address();
  assert.ok(address && typeof address !== "string");
  const browser = await chromium.launch({ headless: false });

  try {
    const page = await browser.newPage({ viewport: { width: 1280, height: 900 } });
    await page.goto(`http://127.0.0.1:${address.port}/auto`);
    const tierRows = page.locator(".auto-tier-row");
    await tierRows.first().waitFor();
    assert.equal(await tierRows.count(), MODELS.length);
    const descriptions = await tierRows.locator("p").allTextContents();
    assert.equal(new Set(descriptions).size, MODELS.length);
    assert.ok(descriptions.every((description) => !description.toLowerCase().includes("per conversation")));
    assert.equal(await page.getByRole("link", { name: "Copilot tier guidance" }).count(), 1);
    await page.screenshot({ path: resolve(SCREENSHOT_DIR, "copilot-auto-guide.png"), fullPage: true });

    await page.getByRole("button", { name: "Models" }).click();
    const modelRows = page.locator(".models-list tbody tr");
    await modelRows.first().waitFor();
    assert.equal(await modelRows.count(), MODELS.length);
    assert.equal(await page.locator(".model-description").count(), MODELS.length);
    await page.screenshot({ path: resolve(SCREENSHOT_DIR, "copilot-auto-models.png"), fullPage: true });
  } finally {
    await browser.close();
    await new Promise((resolveClose, reject) => {
      server.close((error) => (error ? reject(error) : resolveClose()));
    });
  }
});
