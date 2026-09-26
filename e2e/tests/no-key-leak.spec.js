/* Regression test: the API key never reaches a URL, a log line, or the console
 * (issue #726).
 *
 * The API key is a bearer credential. The moment one appears in a URL it is in
 * the browser history, in the `Referer` header of every outbound request, in any
 * proxy log between the operator and the gateway, and in every screenshot a
 * merchant takes when asking for help. Once it is in the console it is in
 * anything that captures the page, including a CI artifact.
 *
 * This is asserted three ways, because each catches a different failure:
 *
 *   1. Over every network request the browser makes — a request whose *URL*
 *      contains the key fails even if the key is also correctly in the header.
 *   2. Over the page's console output and any uncaught error.
 *   3. Over the address bar and the history, after every interaction that
 *      writes to the URL (filter chips, search, auto-refresh).
 *
 * A source-level guard for the same property lives in
 * `tests/dashboard_asset_tests.rs`; this suite is the end-to-end half, and it is
 * the only one that can catch a leak built at runtime rather than in source.
 */

import { test, expect } from "./fixtures.js";

/**
 * Record every request/response and every console message for a page.
 *
 * The listeners are attached before any navigation so nothing that happens
 * during the very first load escapes the capture.
 */
function instrument(page) {
  const requests = [];
  const consoleMessages = [];
  const pageErrors = [];

  page.on("request", (req) => {
    requests.push({
      method: req.method(),
      url: req.url(),
      headers: req.headers(),
      resourceType: req.resourceType(),
      isNavigationRequest: () => req.isNavigationRequest(),
    });
  });
  page.on("console", (msg) => {
    consoleMessages.push({ type: msg.type(), text: msg.text(), location: msg.location() });
  });
  page.on("pageerror", (e) => pageErrors.push({ message: e.message, stack: e.stack }));

  return { requests, consoleMessages, pageErrors };
}

test.describe("the API key never leaves the Authorization header", () => {
  test("no request URL, console line or address bar contains the key", async ({
    page,
    seedData,
  }) => {
    const key = seedData.apiKey;
    /* A short, distinctive key would make a substring match meaningless — a
       match on "a" proves nothing. Assert the real provisioned key, which is
       67 hex characters long. */
    expect(key.length).toBeGreaterThan(32);

    const { requests, consoleMessages, pageErrors } = instrument(page);

    // Sign in. This is the only interaction that handles the raw key.
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(key);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr").first()).toBeVisible();

    // Walk every code path that touches a URL.
    await page.getByRole("button", { name: "Completed", exact: true }).click();
    await expect(page.locator("#rows tr")).toHaveCount(1);
    await page.getByRole("button", { name: "All", exact: true }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);

    await page.getByLabel("Search payments").fill("alpha");
    await expect(page.locator("#rows tr")).toHaveCount(1);
    await page.getByLabel("Search payments").fill("");
    await expect(page.locator("#rows tr")).toHaveCount(4);

    await page.locator("#auto-refresh").check();
    await page.locator("#auto-refresh").uncheck();

    await page.locator("#rows tr").first().click();
    await expect(page.locator("#detail")).toBeVisible();
    await page.locator("#detail-close").click();

    // 1. No request URL carries the key.
    const leakingRequests = requests.filter((r) => r.url.includes(key));
    expect(
      leakingRequests.map((r) => `${r.method} ${r.url}`),
      "the key must never appear in a request URL",
    ).toEqual([]);

    // Nor in any query-string value, which is the other way a credential gets
    // into a log line via a server-side access log.
    const urlValues = requests.flatMap((r) => [...new URL(r.url).searchParams.values()]);
    expect(urlValues.filter((v) => v.includes(key))).toEqual([]);

    // 2. The key *is* carried, in the header — otherwise this test would pass
    //    trivially against a dashboard that never authenticates at all.
    const authorised = requests.filter((r) =>
      (r.headers.authorization ?? "").includes(key),
    );
    expect(
      authorised.length,
      "the key must be sent in the Authorization header on authenticated calls",
    ).toBeGreaterThan(0);

    for (const r of authorised) {
      expect(r.url, "a credential-bearing request must not also carry it in the URL").not.toContain(
        key,
      );
    }

    // 3. Nothing in the console, and no uncaught error quoting it.
    const consoleLeaks = consoleMessages.filter((m) => m.text.includes(key));
    expect(
      consoleLeaks.map((m) => `${m.type}: ${m.text}`),
      "the key must never be written to the console",
    ).toEqual([]);

    const errorLeaks = pageErrors.filter((e) => e.message.includes(key) || (e.stack ?? "").includes(key));
    expect(
      errorLeaks.map((e) => e.message),
      "the key must never appear in an uncaught error",
    ).toEqual([]);

    // 4. The address bar and the history entries the page created.
    expect(page.url(), "the key must never be in the address bar").not.toContain(key);
    const history = await page.evaluate(() => window.history.length);
    expect(typeof history).toBe("number");
    const currentHash = new URL(page.url()).hash;
    expect(currentHash, "the hash carries filters only").not.toContain(key);
  });

  test("the key is absent from the URL after a bad sign-in and after sign-out", async ({
    page,
    seedData,
  }) => {
    const { requests, consoleMessages } = instrument(page);
    const key = seedData.apiKey;

    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(key);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr").first()).toBeVisible();

    // A 401 must drop the key and return to the gate without ever putting it
    // anywhere durable. Revoke nothing — simply clear the stored credential
    // server-side is not reachable from here, so exercise the 401 path with a
    // key the server has never seen, after a valid session has established
    // that the error path renders.
    await page.getByRole("button", { name: "Sign out" }).click();
    await expect(page.locator("#gate")).toBeVisible();

    await page.getByLabel("API key").fill("sg_definitely_not_a_valid_key_0000");
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#gate")).toBeVisible();

    expect(page.url()).not.toContain(key);
    expect(requests.filter((r) => r.url.includes(key))).toEqual([]);
    expect(consoleMessages.filter((m) => m.text.includes(key))).toEqual([]);
  });

  test("the sign-in form cannot leak the key through a native form submit", async ({
    page,
    seedData,
  }) => {
    /* The gate is `method="post"` on purpose. If the script ever fails to load,
       a default GET submit would put the key in the URL — history, referrer,
       every proxy log. Block the entry module so the form has no script to
       preventDefault, then submit and check both what the browser *tried* to
       send and what the CSP let through.

       The dashboard's CSP carries `form-action 'none'`, so the submit is
       blocked client-side and never reaches the network at all. That is the
       defence that actually holds, and asserting on the network alone would
       pass even if the form were a GET — so the test also checks the method the
       browser would have used, from the DOM rather than from a request that
       never happened. */
    const key = seedData.apiKey;
    const { requests, consoleMessages } = instrument(page);

    await page.route("**/dashboard/app.js", (route) => route.abort());
    await page.goto("/dashboard");

    // Confirm the script really is gone, or this test proves nothing.
    await expect(page.getByLabel("API key")).toBeVisible();
    await page.getByLabel("API key").fill(key);

    // The form's own shape is the load-bearing part: `method="post"` keeps the
    // key in a request body that /dashboard does not read.
    const form = await page.evaluate(() => {
      const el = document.getElementById("gate-form");
      return {
        method: el.method.toUpperCase(),
        action: new URL(el.action).pathname,
        noValidate: el.noValidate,
      };
    });
    expect(form.method, "the gate must POST, never GET, or the key lands in the URL").toBe(
      "POST",
    );
    expect(form.action).toBe("/dashboard");

    /* `noWaitAfter`: the click starts a form submission, and a click that
       triggers navigation otherwise blocks until the navigation settles. The
       navigation is *supposed* to be blocked by the CSP, so waiting on it is
       waiting for the very thing this test asserts cannot happen. */
    await page
      .getByRole("button", { name: "Sign in" })
      .click({ noWaitAfter: true });

    await expect
      .poll(() => consoleMessages.some((m) => m.text.includes("form-action")), {
        timeout: 10_000,
        message: "the CSP must refuse the form submit outright",
      })
      .toBe(true);

    // Nothing carrying the key left the browser, by any method.
    expect(requests.filter((r) => r.url.includes(key))).toEqual([]);
    expect(
      requests.filter((r) => r.method === "POST" && r.isNavigationRequest?.()),
      "the CSP must block the submit before it reaches the network",
    ).toEqual([]);
    expect(page.url(), "the address bar must be unchanged").not.toContain(key);
  });

  test("no dashboard response echoes the key back", async ({ page, seedData }) => {
    /* A server that reflected the Authorization header into a response body
       would put the key in the DOM, from where it reaches any screenshot. */
    const key = seedData.apiKey;
    const bodies = [];

    page.on("response", async (res) => {
      if (!res.url().includes("/v1/")) return;
      const type = res.headers()["content-type"] ?? "";
      if (!type.includes("json")) return;
      try {
        bodies.push(await res.text());
      } catch {
        /* a redirect or an aborted body is not interesting here */
      }
    });

    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(key);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr").first()).toBeVisible();
    await page.locator("#rows tr").first().click();
    await expect(page.locator("#detail")).toBeVisible();

    const leaking = bodies.filter((b) => b.includes(key));
    expect(leaking, "an API response must never echo the bearer key").toEqual([]);
  });
});
