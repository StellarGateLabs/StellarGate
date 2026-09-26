/* End-to-end coverage for the keyboard shortcuts (issue #721).
 *
 * `static/keys.js` unit-tests *which* keystroke means which action. This file
 * covers the half a unit test cannot: that the handler is actually wired to the
 * document, that the browser's own defaults are suppressed, and that focus lands
 * somewhere sensible afterwards. Those are exactly the regressions that survive
 * a green `node --test` run.
 */

import { test, expect } from "./fixtures.js";

/** Sign in and return with the list rendered. */
async function signIn(page, seedData) {
  await page.goto("/dashboard");
  await page.getByLabel("API key").fill(seedData.apiKey);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.locator("#rows tr")).toHaveCount(4);
}

/** The row whose memo cell holds `memo`. */
function rowWithMemo(page, memo) {
  return page.locator("#rows tr").filter({
    has: page.locator("td", { hasText: new RegExp(`^${memo}$`) }),
  });
}

test.describe("keyboard shortcuts", () => {
  test("? opens the help overlay and Esc closes it", async ({ page, seedData }) => {
    await signIn(page, seedData);

    const help = page.locator("#help");
    await expect(help).toBeHidden();

    await page.keyboard.press("?");
    await expect(help).toBeVisible();
    // It is a dialog, so assistive technology announces it as one.
    await expect(help).toHaveAttribute("role", "dialog");
    await expect(help).toHaveAttribute("aria-modal", "true");

    // The overlay lists every shortcut the issue asks for, rendered from the
    // same table the matcher uses.
    const rows = help.locator("li.help-row");
    await expect(rows).toHaveCount(6);
    for (const [key, label] of [
      ["r", "Reload payments"],
      ["/", "Focus the search box"],
      ["j", "Select the next payment"],
      ["k", "Select the previous payment"],
      ["Esc", "Close the detail panel"],
      ["?", "Show or hide this help"],
    ]) {
      const row = rows.filter({ hasText: label });
      await expect(row).toHaveCount(1);
      await expect(row.locator("kbd")).toHaveText(key);
    }

    await page.keyboard.press("Escape");
    await expect(help).toBeHidden();
  });

  test("? toggles the overlay closed again", async ({ page, seedData }) => {
    await signIn(page, seedData);
    await page.keyboard.press("?");
    await expect(page.locator("#help")).toBeVisible();
    await page.keyboard.press("?");
    await expect(page.locator("#help")).toBeHidden();
  });

  test("the help overlay is modal over the list", async ({ page, seedData }) => {
    await signIn(page, seedData);
    await page.keyboard.press("?");
    await expect(page.locator("#help")).toBeVisible();

    // j must not move rows behind an open dialog.
    await page.keyboard.press("j");
    await expect(page.locator("#rows tr.row-active")).toHaveCount(0);

    // Nor must r reload behind it.
    await page.keyboard.press("r");
    await expect(page.locator("#rows tr")).toHaveCount(4);

    await page.keyboard.press("Escape");
    await expect(page.locator("#help")).toBeHidden();
  });

  test("clicking outside the help panel closes it", async ({ page, seedData }) => {
    await signIn(page, seedData);
    await page.keyboard.press("?");
    await expect(page.locator("#help")).toBeVisible();

    // A click on the overlay backdrop, well away from the panel itself.
    await page.locator("#help").click({ position: { x: 5, y: 5 } });
    await expect(page.locator("#help")).toBeHidden();
  });

  test("j and k move the highlight and clamp at both ends", async ({
    page,
    seedData,
  }) => {
    await signIn(page, seedData);
    /* The list is `created_at DESC` and the seed fixes those timestamps, so
       `rowOrder` is the rendered order and a spec can say which payment the
       first `j` lands on. Deriving the expectation from the seed rather than
       hard-coding a name is what keeps this from being flaky. */
    const [first, second] = seedData.rowOrder;

    // Nothing is highlighted to begin with.
    await expect(page.locator("#rows tr.row-active")).toHaveCount(0);

    await page.keyboard.press("j");
    await expect(page.locator("#rows tr.row-active")).toHaveCount(1);
    await expect(page.locator("#rows tr.row-active")).toContainText(first.memo);

    await page.keyboard.press("j");
    await expect(page.locator("#rows tr.row-active")).toContainText(second.memo);

    await page.keyboard.press("k");
    await expect(page.locator("#rows tr.row-active")).toContainText(first.memo);

    // Clamps at the first row rather than wrapping.
    await page.keyboard.press("k");
    await expect(page.locator("#rows tr.row-active")).toContainText(first.memo);

    // And at the last: past the end of the list the highlight stays put.
    for (let i = 0; i < 6; i++) await page.keyboard.press("j");
    await expect(page.locator("#rows tr")).toHaveCount(4);
    const highlighted = await page.locator("#rows tr.row-active").allTextContents();
    expect(highlighted, "exactly one row is ever highlighted").toHaveLength(1);
    expect(highlighted[0]).toContain(seedData.rowOrder.at(-1).memo);
  });

  test("the highlight is announced to assistive technology", async ({
    page,
    seedData,
  }) => {
    await signIn(page, seedData);
    // The j/k highlight is otherwise a purely visual change.
    await expect(page.locator("#rows-status")).toHaveAttribute("aria-live", "polite");

    await page.keyboard.press("j");
    await expect(page.locator("#rows-status")).toContainText("Row 1 of 4");
    await expect(page.locator("#rows-status")).toContainText(seedData.rowOrder[0].memo);
  });

  test("Enter opens the highlighted payment and Esc closes the drawer", async ({
    page,
    seedData,
  }) => {
    await signIn(page, seedData);
    const target = seedData.rowOrder[1];

    await page.keyboard.press("j");
    await page.keyboard.press("j");
    await expect(page.locator("#rows tr.row-active")).toContainText(target.memo);

    await page.keyboard.press("Enter");
    const detail = page.locator("#detail");
    await expect(detail).toBeVisible();
    await expect(detail).toContainText(target.memo);

    await page.keyboard.press("Escape");
    await expect(detail).toBeHidden();
  });

  test("/ focuses the search box and the letters then stop being shortcuts", async ({
    page,
    seedData,
  }) => {
    await signIn(page, seedData);

    await page.keyboard.press("/");
    await expect(page.getByLabel("Search payments")).toBeFocused();

    /* Typing a word made entirely of shortcut letters must not fire any of
       them: this is the case the "ignored while typing" rule exists for. The
       string is a search term, not one of the fixtures' memos, so matching
       nothing is the proof that no shortcut hijacked the keystrokes. */
    await page.keyboard.type("rjk");
    await expect(page.getByLabel("Search payments")).toHaveValue("rjk");
    // The list narrowed to nothing rather than reloading or moving a row.
    await expect(page.locator("#rows tr")).toHaveCount(0);
    await expect(page.locator("#empty")).toBeVisible();

    await page.keyboard.press("Escape");
    for (let i = 0; i < 3; i++) await page.keyboard.press("Backspace");
    await expect(page.locator("#rows tr")).toHaveCount(4);
  });

  test("shortcuts are ignored while the API key field has focus", async ({ page }) => {
    await page.goto("/dashboard");
    const keyField = page.getByLabel("API key");
    await keyField.click();
    await expect(keyField).toBeFocused();

    // "r" must land in the field, not trigger a reload.
    await page.keyboard.type("r");
    await expect(keyField).toHaveValue("r");

    // The help overlay must not open from the gate either.
    await page.keyboard.press("?");
    await expect(page.locator("#help")).toBeHidden();
  });

  test("r reloads the list", async ({ page, seedData }) => {
    await signIn(page, seedData);

    /* Count only the list call, not the summary or the detail fetch, so a pass
       means the list was actually re-requested rather than the page merely
       having re-rendered. */
    let listCalls = 0;
    await page.route("**/v1/payments?*", async (route, request) => {
      if (new URL(request.url()).searchParams.has("limit")) listCalls++;
      await route.continue();
    });

    await expect.poll(() => listCalls, { timeout: 10_000 }).toBe(0);
    await page.keyboard.press("r");
    await expect.poll(() => listCalls, { timeout: 10_000 }).toBeGreaterThan(0);

    /* Two presses, and the row count must still be the seeded one. This is the
       assertion that catches a reload implemented as an append: a single press
       looks fine, the second doubles the list. */
    await expect(page.locator("#rows tr")).toHaveCount(seedData.rowOrder.length);
    await page.keyboard.press("r");
    await expect.poll(() => listCalls, { timeout: 10_000 }).toBeGreaterThan(1);
    await expect(page.locator("#rows tr")).toHaveCount(seedData.rowOrder.length);
  });

  test("the page does not scroll when space is pressed on a highlighted row", async ({
    page,
    seedData,
  }) => {
    await signIn(page, seedData);

    // A row must be focused for the row's own Space handler to be in play.
    // Focus first, then measure: `focus()` scrolls the element into view, and
    // on a mobile card layout the first row is far above the fold — so
    // measuring before focusing would attribute that scroll to the Space key.
    await page.locator("#rows tr").first().focus();
    await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
    const before = await page.evaluate(() => window.scrollY);

    await page.keyboard.press(" ");

    // Space on a focused row opens the payment rather than scrolling the page.
    await expect(page.locator("#detail")).toBeVisible();
    const after = await page.evaluate(() => window.scrollY);
    expect(after, "Space must not scroll the page").toBe(before);
  });

  test("the shortcut button opens the overlay too", async ({ page, seedData }) => {
    await signIn(page, seedData);
    await page.locator("#help-open").click();
    await expect(page.locator("#help")).toBeVisible();
    await expect(page.locator("#help-close")).toBeFocused();

    // Closing returns focus to the opener, not to the top of the document.
    await page.keyboard.press("Escape");
    await expect(page.locator("#help")).toBeHidden();
    await expect(page.locator("#help-open")).toBeFocused();
  });
});
