/* End-to-end smoke test for the operator dashboard (issue #724).
 *
 * Sign in, filter, open a payment, redeliver a webhook — driven through a real
 * browser against a real server with a seeded database, because each of those
 * steps crosses a boundary a unit test cannot reach: a module the router
 * forgets to serve, a CSP that blocks an import, a filter the API rejects, a
 * confirm dialog that swallows a click.
 */

import { test, expect } from "./fixtures.js";

test.describe("dashboard smoke", () => {
  test("every module loads and the dashboard signs in", async ({ page }) => {
    /* A module that 404s does not throw a visible error — `type="module"`
       simply fails to evaluate and the page sits on the sign-in form forever,
       looking like a credentials problem. Assert on the requests themselves so
       the failure names the missing file. */
    const moduleFailures = [];
    page.on("response", (res) => {
      if (res.url().includes("/dashboard/") && !res.ok()) {
        moduleFailures.push(`${res.status()} ${res.url()}`);
      }
    });
    const pageErrors = [];
    page.on("pageerror", (e) => pageErrors.push(e.message));

    await page.goto("/dashboard");

    for (const [path, contentType] of [
      ["/dashboard/app.css", "text/css"],
      ["/dashboard/app.js", "javascript"],
      ["/dashboard/format.js", "javascript"],
      ["/dashboard/session.js", "javascript"],
      ["/dashboard/state.js", "javascript"],
      ["/dashboard/keys.js", "javascript"],
    ]) {
      const res = await page.request.get(path);
      expect(res.status(), `${path} must be served`).toBe(200);
      expect(res.headers()["content-type"], `${path} content type`).toContain(
        contentType,
      );
    }

    await expect(page.locator("#gate")).toBeVisible();
    await expect(page.locator("#app")).toBeHidden();
    expect(moduleFailures, "no dashboard asset may fail to load").toEqual([]);
    expect(pageErrors, "the entry module must evaluate cleanly").toEqual([]);
  });

  /** The row whose memo cell holds `memo`. Scoped to the memo cell so a label
   *  elsewhere on the page cannot satisfy the locator. */
  function rowWithMemo(page, memo) {
    return page.locator("#rows tr").filter({
      has: page.locator("td", { hasText: new RegExp(`^${memo}$`) }),
    });
  }

  test("signs in and lists the seeded payments", async ({ page, seedData }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();

    await expect(page.locator("#app")).toBeVisible();
    await expect(page.locator("#gate")).toBeHidden();

    const rows = page.locator("#rows tr");
    await expect(rows).toHaveCount(4);

    // Every seeded memo is present, so a filter silently dropping rows is
    // visible here rather than as a mysteriously short list.
    for (const memo of ["alpha", "bravo", "charlie", "delta"]) {
      await expect(rowWithMemo(page, `e2e-order-${memo}`)).toHaveCount(1);
    }

    // The status pills reflect the seeded statuses, which is what the summary
    // cards and the filters are derived from.
    await expect(rowWithMemo(page, "e2e-order-alpha")).toContainText("completed");
    await expect(rowWithMemo(page, "e2e-order-delta")).toContainText("pending");

    // The API is healthy, so the health pill resolves rather than sitting on
    // "checking…" forever.
    await expect(page.locator("#health")).toHaveText("healthy");
  });

  test("the summary is scoped to the signed-in merchant", async ({
    page,
    seedData,
  }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr").first()).toBeVisible();

    /* The summary aggregates one merchant's payments, so it must be behind the
       same credential check as the list. An unauthenticated call used to reach
       the handler with no merchant identity and answer 500. */
    const unauth = await page.request.get("/v1/payments/summary");
    expect(unauth.status()).toBe(401);

    // And with a credential it returns this merchant's own totals.
    const authed = await page.request.get("/v1/payments/summary", {
      headers: { Authorization: `Bearer ${seedData.apiKey}` },
    });
    expect(authed.status()).toBe(200);
    expect((await authed.json()).summary).toBeTruthy();
  });

  test("rejects a bad key without leaving a broken page", async ({ page }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill("sg_not_a_real_key_at_all");
    await page.getByRole("button", { name: "Sign in" }).click();

    // Either the gate reports the rejection or it bounces straight back; both
    // are acceptable, a blank page is not.
    await expect(page.locator("#gate")).toBeVisible();
    await expect(page.locator("#app")).toBeHidden();
  });

  test("filters the list by status", async ({ page, seedData }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);

    await page.getByRole("button", { name: "Completed", exact: true }).click();
    await expect(page.locator("#rows tr")).toHaveCount(1);
    await expect(rowWithMemo(page, "e2e-order-alpha")).toHaveCount(1);

    await page.getByRole("button", { name: "Pending", exact: true }).click();
    await expect(page.locator("#rows tr")).toHaveCount(1);
    await expect(rowWithMemo(page, "e2e-order-delta")).toHaveCount(1);

    // The filter lives in the URL hash, so the view is shareable — and the
    // hash is a place a credential must never appear.
    await expect(page).toHaveURL(/#status=pending/);

    await page.getByRole("button", { name: "All", exact: true }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);
  });

  test("searches the loaded rows", async ({ page, seedData }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);

    const search = page.getByLabel("Search payments");
    await search.fill("charlie");
    await expect(page.locator("#rows tr")).toHaveCount(1);
    await expect(rowWithMemo(page, "e2e-order-charlie")).toHaveCount(1);

    // The clear button only appears when there is something to clear, and
    // clearing restores the full list.
    const clear = page.getByRole("button", { name: "Clear search" });
    await expect(clear).toBeVisible();
    await clear.click();
    await expect(page.locator("#rows tr")).toHaveCount(4);
  });

  test("opens a payment and shows its detail panel", async ({ page, seedData }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);

    await rowWithMemo(page, "e2e-order-alpha").click();

    const detail = page.locator("#detail");
    await expect(detail).toBeVisible();
    await expect(rowWithMemo(page, "e2e-order-alpha")).toHaveCount(1);

    // Detail fields that only exist because of specific features.
    await expect(detail).toContainText("Network");
    await expect(detail).toContainText("Stellar");
    await expect(detail).toContainText("Asset issuer");
    await expect(detail).toContainText("42.5 XLM");

    // The transaction hash links out to an explorer, in a new tab, with
    // `rel` set so the opened page cannot reach back through window.opener.
    const link = detail.getByRole("link");
    await expect(link).toHaveAttribute("href", /stellar\.expert/);
    await expect(link).toHaveAttribute("target", "_blank");
    await expect(link).toHaveAttribute("rel", /noopener/);

    // Closing returns to the list.
    await page.locator("#detail-close").click();
    await expect(detail).toBeHidden();
  });

  test("lists webhook deliveries and redelivers one", async ({
    page,
    seedData,
    webhooks,
  }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);

    // The completed payment carries the seeded failed delivery.
    await rowWithMemo(page, "e2e-order-alpha").click();
    const detail = page.locator("#detail");
    await expect(detail).toBeVisible();

    const delivery = detail.locator("li.delivery").first();
    await expect(delivery).toBeVisible();
    await expect(delivery).toContainText("payment.completed");
    await expect(delivery).toContainText("failed");
    await expect(delivery).toContainText("attempt 3");

    webhooks.reset();

    /* Redelivery is behind a confirm dialog (#705). Accepting it must reach
       the merchant's endpoint — so assert on what the receiver got, not on the
       dashboard's own bookkeeping. The seeded delivery points at the
       receiver's /fail path, so the endpoint refuses it, which is the state a
       merchant is actually redriving from. */
    const redeliver = delivery.getByRole("button", { name: "Redeliver" });
    page.once("dialog", (d) => d.accept());
    await redeliver.click();

    // The merchant's endpoint was actually called.
    await expect.poll(() => webhooks.received.length, { timeout: 20_000 }).toBe(1);
    const [hit] = webhooks.received;
    expect(hit.method).toBe("POST");
    expect(hit.path).toContain("/fail");
    // And the gateway signed it, so the merchant can trust it.
    expect(hit.signatureValid, "the redelivery must carry a valid HMAC signature").toBe(true);
    expect(hit.headers["x-stellargate-event"]).toBe("payment.completed");

    /* A refused redelivery answers 502, and the operator needs to be told
       rather than left staring at a button that did nothing (#705). */
    await expect(page.locator("#deliveries-error")).toBeVisible();
    await expect(page.locator("#deliveries-error")).not.toBeEmpty();
    // The button returns to its idle label so the attempt can be retried.
    await expect(redeliver).toHaveText("Redeliver");
    await expect(redeliver).toBeEnabled();
  });

  test("a redelivery to a healthy endpoint succeeds", async ({
    page,
    seedData,
    webhooks,
  }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);

    // `delta` carries a delivery that already succeeded, pointing at the
    // receiver's healthy path.
    await rowWithMemo(page, "e2e-order-delta").click();
    const detail = page.locator("#detail");
    await expect(detail).toBeVisible();

    const delivery = detail.locator("li.delivery").first();
    await expect(delivery).toContainText("delivered");
    await expect(delivery).toContainText("payment.created");

    webhooks.reset();
    const redeliver = delivery.getByRole("button", { name: "Redeliver" });
    page.once("dialog", (d) => d.accept());
    await redeliver.click();

    /* A delivery that already succeeded is refused with 409 unless the caller
       opts in with `?force=true` (issue #236) — a double-click must not send a
       duplicate event. The dashboard's button does not opt in, so the attempt
       is correctly rejected, nothing reaches the receiver, and the operator is
       told why. */
    await expect(page.locator("#deliveries-error")).toBeVisible();
    await expect(page.locator("#deliveries-error")).toContainText("already");
    expect(webhooks.received, "no duplicate event may be sent").toEqual([]);
    await expect(redeliver).toHaveText("Redeliver");
    await expect(redeliver).toBeEnabled();
  });

  test("a forced redelivery of a delivered event reaches the receiver", async ({
    page,
    seedData,
    webhooks,
  }) => {
    /* The counterpart to the 409 above, via the API: `?force=true` is the
       documented opt-in, and it must actually deliver — otherwise the guard
       above would be indistinguishable from the endpoint being broken. */
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);

    webhooks.reset();
    const res = await page.request.post(
      `/v1/payments/${seedData.payments.pending.id}` +
        `/webhooks/${seedData.delivery.delivered.id}/redeliver?force=true`,
      { headers: { Authorization: `Bearer ${seedData.apiKey}` } },
    );
    expect(res.status()).toBe(200);

    await expect.poll(() => webhooks.received.length, { timeout: 20_000 }).toBe(1);
    const [hit] = webhooks.received;
    expect(hit.method).toBe("POST");
    expect(hit.path).not.toContain("/fail");
    expect(hit.signatureValid, "a redelivery must be signed for the receiver").toBe(true);
    expect(hit.headers["x-stellargate-event"]).toBe("payment.created");
  });

  test("cancelling the redelivery confirm sends nothing", async ({
    page,
    seedData,
    webhooks,
  }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#rows tr")).toHaveCount(4);

    await rowWithMemo(page, "e2e-order-alpha").click();
    await expect(page.locator("#detail")).toBeVisible();

    webhooks.reset();
    page.once("dialog", (d) => d.dismiss());
    await page.locator("li.delivery").first().getByRole("button", { name: "Redeliver" }).click();

    // Dismiss the dialog, then confirm nothing was sent and the button is
    // still usable rather than stuck on "Sending…".
    await page.waitForTimeout(500);
    expect(webhooks.received).toEqual([]);
    await expect(
      page.locator("li.delivery").first().getByRole("button", { name: "Redeliver" }),
    ).toBeEnabled();
  });

  test("dismissing a payment with no deliveries says so", async ({ page, seedData }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();

    await rowWithMemo(page, "e2e-order-bravo").click();
    await expect(page.locator("#detail")).toBeVisible();
    await expect(page.locator("#deliveries-empty")).toBeVisible();
  });

  test("signing out returns to the gate and forgets the key", async ({ page, seedData }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#app")).toBeVisible();

    await page.getByRole("button", { name: "Sign out" }).click();
    await expect(page.locator("#gate")).toBeVisible();
    await expect(page.locator("#app")).toBeHidden();

    // The stored key is gone, so a reload does not silently sign back in.
    await page.reload();
    await expect(page.locator("#gate")).toBeVisible();
  });

  test("a remembered session is restored on reload", async ({ page, seedData }) => {
    await page.goto("/dashboard");
    await page.getByLabel("API key").fill(seedData.apiKey);
    await page.getByLabel("Keep me signed in on this device").check();
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.locator("#app")).toBeVisible();

    await page.reload();
    // Restored without re-entering the key.
    await expect(page.locator("#app")).toBeVisible();
    await expect(page.locator("#rows tr")).toHaveCount(4);
  });
});
