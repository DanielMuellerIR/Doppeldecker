#!/usr/bin/env node

/** Reproduzierbare README-Screenshots gegen die echte Oberfläche und die
 * Demo-Daten des Mock-Servers (`?demo`). Erzeugt hochauflösende PNGs (Retina,
 * Hell und Dunkel) in `screenshots/`. Verändert keine Nutzerdaten und kein
 * echtes App-Profil — es rendert nur `frontend/` über die kontrollierte
 * Tauri-Mockbrücke.
 *
 * Aufruf: `node tools/screenshots.mjs` (Node 20+, Playwright + Chromium).
 */

import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import { createServer } from "node:net";
import { once } from "node:events";
import { mkdir } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { createBrowserCleanup } from "./browser-cleanup.mjs";

const require = createRequire(import.meta.url);
let playwright;
try {
  playwright = require("playwright");
} catch (error) {
  throw new Error("Playwright fehlt. Zuerst `pnpm install` und `pnpm exec playwright install chromium` ausführen.", { cause: error });
}

const ROOT = new URL("..", import.meta.url);
const OUT = fileURLToPath(new URL("screenshots/", ROOT));

async function freePort() {
  const probe = createServer();
  probe.listen(0, "127.0.0.1");
  await once(probe, "listening");
  const { port } = probe.address();
  probe.close();
  await once(probe, "close");
  return port;
}

async function waitForServer(url, cleanup) {
  for (let attempt = 0; attempt < 80; attempt += 1) {
    cleanup.checkServer();
    try {
      if ((await fetch(url)).ok) return;
    } catch (_) {
      // Der Server braucht üblicherweise nur wenige Schleifendurchläufe zum Start.
    }
    await new Promise(resolve => setTimeout(resolve, 25));
  }
  throw new Error("Mock-Server wurde nicht rechtzeitig erreichbar.");
}

// Jede Ansicht stellt ihren Zustand über die echte App-Verdrahtung her (native
// Menü-Ereignisse), damit die Screenshots zeigen, was Nutzer wirklich sehen.
const VIEWS = [
  {
    name: "overview",
    async setup() {},
  },
  {
    name: "sync",
    async setup(page) {
      await page.evaluate(() => window.__MOCK_EMIT__("menu", "sync_mode"));
      await page.locator("#sync-comparison-list [role=option]").first().waitFor();
      await page.waitForTimeout(300);
    },
  },
];

const THEMES = ["light", "dark"];

const port = await freePort();
const baseUrl = `http://127.0.0.1:${port}`;
const server = spawn("python3", ["tools/mock-ui-server.py", "--port", String(port)], {
  cwd: fileURLToPath(ROOT),
  stdio: ["ignore", "pipe", "pipe"],
});
let serverStderr = "";
server.stderr.setEncoding("utf8");
server.stderr.on("data", chunk => { serverStderr += chunk; });
const cleanup = createBrowserCleanup(server);

let browser;
const written = [];
try {
  await waitForServer(baseUrl, cleanup);
  await mkdir(OUT, { recursive: true });
  const executablePath = process.env.DOPPELDECKER_PLAYWRIGHT_EXECUTABLE || undefined;
  browser = await cleanup.launch(playwright.chromium, { headless: true, executablePath });

  for (const theme of THEMES) {
    const context = await browser.newContext({
      colorScheme: theme,
      viewport: { width: 1360, height: 800 },
      deviceScaleFactor: 2,
    });
    // Vor dem Rendern: etwas breitere Seitenleiste (volle Verbindungsnamen) und
    // festes Theme, damit die Aufnahme reproduzierbar ist.
    await context.addInitScript((chosenTheme) => {
      localStorage.setItem("doppeldecker-sidebar-width", "214");
      localStorage.setItem("doppeldecker-theme", chosenTheme);
    }, theme);

    for (const view of VIEWS) {
      const page = await context.newPage();
      const problems = [];
      page.on("pageerror", error => problems.push(error.message));
      page.on("console", message => { if (message.type() === "error") problems.push(message.text()); });

      await page.goto(`${baseUrl}/?demo=1`, { waitUntil: "networkidle" });
      await page.getByRole("listbox", { name: /Linke Dateiliste/ }).waitFor();
      // Neutraler Fensterkopf für die Aufnahme: ohne den macOS-spezifischen
      // Ampel-Freiraum beginnt die Seitenleiste direkt oben (die echten
      // Ampelknöpfe fehlen im Browser ohnehin und hinterließen sonst nur Leerraum).
      await page.evaluate(() => { document.documentElement.dataset.platform = "linux"; });
      await page.waitForTimeout(250);
      await view.setup(page);

      const mockErrors = await page.evaluate(() => window.__DOPPELDECKER_MOCK_ERRORS__ || []);
      if (mockErrors.length || problems.length) {
        throw new Error(`Fehler beim Rendern von ${view.name}/${theme}: ${[...mockErrors, ...problems].join(" | ")}`);
      }

      const path = `${OUT}${view.name}-${theme}.png`;
      await page.screenshot({ path });
      written.push(`screenshots/${view.name}-${theme}.png`);
      await page.close();
    }
    await context.close();
  }
  console.log("Geschrieben:\n  " + written.join("\n  "));
} catch (error) {
  if (serverStderr.trim()) console.error(serverStderr.trim());
  console.error(String(error?.stack || error));
  process.exitCode = 1;
} finally {
  await cleanup.close();
}
