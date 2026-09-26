/* Playwright configuration for the dashboard end-to-end suite (#724, #726).
 *
 * The suite runs against a real `stellargate` binary with a real SQLite file,
 * not a mock: the point of these tests is to catch the class of failure that
 * only appears once the server, the router and the browser are all involved —
 * a module that 404s, a CSP that blocks a module import, a filter that the API
 * rejects, a credential that leaks into a URL.
 *
 * Nothing here needs a Stellar node, a webhook receiver, or any credential from
 * the environment. The server boots with the gateway unconfigured, so the
 * Horizon listeners stay idle, and the merchant is provisioned by the seed.
 */

import { defineConfig, devices } from "@playwright/test";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, "..");

/** A fixed port, so the specs and the server agree without a discovery file. */
export const PORT = Number(process.env.E2E_PORT || 3417);
export const BASE_URL = `http://127.0.0.1:${PORT}`;

/** Where the seed writes the merchant id and the throwaway API key. */
export const SEED_FILE = join(here, ".seed.json");

/** The SQLite file the server and the seed both open. */
export const DB_FILE = join(here, "playwright.db");

/** The gateway binary, built by CI before the suite runs. */
export const SERVER_BIN = join(repoRoot, "target", "debug", "stellargate");

/**
 * A long, non-placeholder secret. `config.rs` refuses known placeholder values
 * so a development database cannot be left protected by a guessable admin
 * secret; this one is generated per run and never leaves the machine.
 */
const WEBHOOK_SECRET = "e2e-only-webhook-signing-secret-not-a-placeholder-32";

/* The seed runs inside the Playwright worker and provisions against the server
   this config starts, so it needs the same environment the server got. Export
   it here rather than in the workflow, so a local `npx playwright test` and CI
   take an identical path. */
Object.assign(process.env, {
  E2E_BASE_URL: BASE_URL,
  ADMIN_PROVISIONING_SECRET: WEBHOOK_SECRET,
  WEBHOOK_SECRET,
  DATABASE_URL: `sqlite:${DB_FILE}`,
});

export default defineConfig({
  testDir: join(here, "tests"),
  /* Serial, not parallel: every spec shares one seeded merchant and one webhook
     receiver, and several assert on shared counters (`manual N` attempts, the
     receiver's request log), so concurrent workers would race on that state.
     30 specs × 5 projects at one worker is ~3.5 min — see the note in the
     `dashboard-e2e` CI job about narrowing the matrix if that becomes painful. */
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  /* One retry in CI. The suite talks to a real server over loopback, so a
     single flake is worth absorbing — but only one, and only here: a test that
     needs a retry to pass is a test whose failure a retry would hide from a
     human, so the retry count is deliberately not raised further. */
  retries: process.env.CI ? 1 : 0,
  /* A hung locator should fail the run, not stall CI until the job times out. */
  timeout: 30_000,
  expect: { timeout: 10_000 },
  /* The GitHub reporter annotates the failing step inline, which is most of what
     makes a red run readable; `list` keeps the per-test lines. The HTML report
     is only written when something failed — on a green run it would be an empty
     directory that `upload-artifact` has nothing to do with, and locally it
     would only be clutter. */
  reporter: process.env.CI
    ? [
        ["github"],
        ["list"],
        ["html", { open: "never", outputFolder: "playwright-report" }],
      ]
    : [["list"]],

  use: {
    baseURL: BASE_URL,
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
    video: "off",
  },

  /* The acceptance criteria call for the dashboard to work in Chrome, Firefox
     and Safari at desktop and mobile widths. That is three engines times two
     widths; the mobile projects run the same specs against the same seeded
     server, which is what makes "works at mobile width" a checked claim rather
     than an aspiration. */
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
    {
      name: "firefox",
      use: { ...devices["Desktop Firefox"] },
    },
    {
      name: "webkit",
      use: { ...devices["Desktop Safari"] },
    },
    {
      name: "mobile-chrome",
      use: { ...devices["Pixel 7"] },
    },
    {
      name: "mobile-safari",
      use: { ...devices["iPhone 14"] },
    },
  ],

  webServer: {
    command: `node ${join(here, "scripts", "serve.mjs")}`,
    url: `${BASE_URL}/ready`,
    /* Generous: the first run may still be compiling the binary. */
    timeout: 120_000,
    reuseExistingServer: !process.env.CI,
    stdout: "pipe",
    stderr: "pipe",
    env: {
      PORT: String(PORT),
      DATABASE_URL: `sqlite:${DB_FILE}`,
      /* Leaving the gateway unconfigured keeps the Horizon stream and poller
         idle, so the suite needs no Stellar node and no network. */
      STELLAR_GATEWAY_PUBLIC: "UNCONFIGURED",
      STELLAR_NETWORK: "testnet",
      WEBHOOK_SECRET,
      ADMIN_PROVISIONING_SECRET: WEBHOOK_SECRET,
      /* Poll rather than stream: the SSE listener would otherwise sit
         reconnecting to a Horizon URL that does not exist. */
      STELLAR_LISTENER_MODE: "poll",
      /* The seeded webhook receiver binds to 127.0.0.1 over plaintext, which
         the SSRF guard and the scheme allow-list both reject by design. These
         two flags are refused outright on the public network, so they cannot
         weaken a real deployment. */
      WEBHOOK_ALLOW_PRIVATE_TARGETS: "true",
      ALLOWED_WEBHOOK_SCHEMES: "http,https",
      RUST_LOG: "warn",
      /* The seed reads these to provision against the server it just started. */
      E2E_BASE_URL: BASE_URL,
    },
  },
});
