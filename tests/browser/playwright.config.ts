import { defineConfig } from "@playwright/test";

const port = Number(process.env.OMG_BROWSER_PORT_BASE ?? 18290) + 1;
const out = "../../target/browser-tests";

// Chromium only, one worker: the journey builds shared state the accessibility pass reuses.
export default defineConfig({
  testDir: ".",
  outputDir: `${out}/results`,
  globalSetup: "./global-setup.ts",
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: !!process.env.CI,
  timeout: 60_000,
  expect: { timeout: 10_000 },
  reporter: process.env.CI ? [["list"], ["html", { open: "never", outputFolder: `${out}/report` }]] : [["list"]],
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    browserName: "chromium",
    viewport: { width: 1440, height: 900 },
    // Disposable database only; one-time keys in traces are dropped with it.
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    { name: "journey", testMatch: /journey\.spec\.ts/ },
    { name: "accessibility", testMatch: /accessibility\.spec\.ts/, dependencies: ["journey"] },
  ],
});
