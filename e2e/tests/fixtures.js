/* Shared Playwright fixtures.
 *
 * The seed runs once per worker and its output is exposed as a test fixture, so
 * every spec gets the same merchant without re-provisioning. The API key is
 * deliberately *not* logged, snapshotted into `test.info().attach`, or written
 * into a reporter annotation anywhere in this file: the value only ever reaches
 * a `page.fill()` call (#726).
 */

import { test as base, expect } from "@playwright/test";
import { seed } from "../fixtures/seed.mjs";

let seedPromise = null;

/** Seed once per worker process; reuse the result across specs. */
function ensureSeeded() {
  if (!seedPromise) {
    seedPromise = seed().catch((e) => {
      /* Clear the memoised rejection so a retry in the same worker is possible
         rather than replaying the same failure forever. */
      seedPromise = null;
      throw e;
    });
  }
  return seedPromise;
}

export const test = base.extend({
  /* Worker-scoped: the database is shared state and a second provision would
     just add noise. */
  seedData: [
    async ({}, use) => {
      await use(await ensureSeeded());
    },
    { scope: "worker" },
  ],

  /* The local webhook receiver the seeded payments point at. Specs assert on
     what it actually received, so a redelivery is verified end to end rather
     than by the dashboard's own bookkeeping. */
  webhooks: async ({ seedData }, use) => {
    await use(seedData.receiver);
  },

  /* The credential under test. Exposed as a fixture rather than read from
     `.seed.json` inline so the "this value must never be printed" discipline
     lives in one place. */
  apiKey: async ({ seedData }, use) => {
    await use(seedData.apiKey);
  },

  /* A page that has already signed in and is looking at the payments list. */
  signedIn: async ({ page, apiKey }, use) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    // The list is the proof the session is live, not just that the gate hid.
    await expect(page.locator("#rows tr").first()).toBeVisible();
    await use(page);
  },
});

export { expect };
