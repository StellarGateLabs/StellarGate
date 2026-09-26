#!/usr/bin/env node
/* Seed the end-to-end database with a merchant, its payments, and a webhook
 * delivery to redeliver (issue #724).
 *
 * Three sources of truth, deliberately:
 *
 *   - The merchant and its payments are created through the *public REST API*,
 *     with an admin secret and a bearer token exactly as an operator would.
 *     That means the seed also proves the API works, and a payment fixture can
 *     never drift from the shape the API actually accepts.
 *
 *   - The payments' settled statuses are written straight into SQLite. The API
 *     only ever creates `pending` intents; settlement is the Horizon listener's
 *     job, and waiting for it would mean a funded Stellar account and a
 *     minutes-long suite.
 *
 *   - The webhook delivery row is written into SQLite too. A delivery only
 *     exists once a webhook has been dispatched, and the seeded ones are
 *     pre-dispatch history — which is the only way to reach the redelivery UI
 *     without settling a payment on-chain.
 *
 * The merchant's API key lands in `e2e/.seed.json`, a git-ignored file the specs
 * read. It is minted per run, it is worthless the moment the run ends, and it is
 * never printed or logged — see `tests/no-key-leak.spec.js` for why that matters
 * (#726).
 */

import { writeFileSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { DatabaseSync } from "node:sqlite";
import { startReceiver } from "./receiver.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const e2eRoot = join(here, "..");

export const SEED_FILE = join(e2eRoot, ".seed.json");

/** Resolved from the same env the Playwright `webServer` block sets. */
export const DB_FILE =
  (process.env.DATABASE_URL || "").replace(/^sqlite:/, "") ||
  join(e2eRoot, "playwright.db");

const BASE_URL = process.env.E2E_BASE_URL || "http://127.0.0.1:3417";
const ADMIN_SECRET = process.env.ADMIN_PROVISIONING_SECRET || "";

async function api(path, { method = "GET", token, body, admin } = {}) {
  const headers = { Accept: "application/json" };
  if (token) headers.Authorization = `Bearer ${token}`;
  if (admin) headers["X-Admin-Secret"] = admin;

  const res = await fetch(`${BASE_URL}/v1${path}`, {
    method,
    headers: body ? { ...headers, "Content-Type": "application/json" } : headers,
    body: body ? JSON.stringify(body) : undefined,
  });

  const text = await res.text();
  let parsed = {};
  try {
    parsed = text ? JSON.parse(text) : {};
  } catch {
    throw new Error(
      `${method} ${path} returned non-JSON (${res.status}): ${text.slice(0, 200)}`,
    );
  }
  if (!res.ok) {
    /* The response body never carries the key back — the API returns only the
       prefix — so the message is safe to print even when it reaches a CI log. */
    throw new Error(
      `${method} ${path} failed (${res.status}): ${parsed.error ?? text.slice(0, 200)}`,
    );
  }
  return parsed;
}

/** Wait for the server to answer /ready before provisioning anything. */
async function waitForServer(timeoutMs = 60_000) {
  const deadline = Date.now() + timeoutMs;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`${BASE_URL}/ready`);
      if (res.ok) return;
      lastError = `status ${res.status}`;
    } catch (e) {
      lastError = e.message;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(
    `the gateway never became ready at ${BASE_URL} (${lastError})`,
  );
}

/** Create a pending payment intent through the public API. */
function createPayment(token, { amount, webhookUrl }) {
  return api("/payments", {
    method: "POST",
    token,
    body: {
      amount,
      asset: "XLM",
      ...(webhookUrl ? { webhook_url: webhookUrl } : {}),
    },
  });
}

/**
 * The gateway, not the caller, chooses a payment's memo — it has to be unique
 * and it is what the payer puts on the transaction. So the readable label the
 * specs select rows by is written afterwards, replacing the generated value.
 *
 * This is also what makes the row assertions readable: without it every row
 * shows an opaque `D8303821` and a spec has to match on amount or id.
 */
function setMemo(db, paymentId, memo) {
  db.prepare("UPDATE payments SET memo = ? WHERE id = ?").run(memo, paymentId);
}

/** Backdate a payment so the list's `created_at DESC` order is deterministic. */
function setCreatedAt(db, paymentId, createdAt) {
  db.prepare("UPDATE payments SET created_at = ? WHERE id = ?").run(
    createdAt,
    paymentId,
  );
}

/** Overwrite an intent's status, standing in for on-chain settlement. */
function setStatus(db, paymentId, status, extra = {}) {
  const columns = Object.keys(extra);
  const assignments = ["status = ?", ...columns.map((c) => `${c} = ?`)];
  const values = [status, ...columns.map((c) => extra[c]), paymentId];
  db.prepare(`UPDATE payments SET ${assignments.join(", ")} WHERE id = ?`).run(
    ...values,
  );
}

/**
 * Insert a webhook delivery row.
 *
 * `failed` is the realistic case: the receiver was down, the gateway gave up,
 * and an operator is now deciding whether to redeliver. That is the flow #724
 * asks the suite to cover, and pointing it at the local receiver's `/fail` path
 * means the redelivery genuinely fails — the same path a merchant hits in
 * production when the endpoint is still broken.
 */
function insertDelivery(db, { id, paymentId, merchantId, event, url, status, attempts }) {
  const now = new Date().toISOString().replace(/\.\d+Z$/, "Z");
  const payload = JSON.stringify({
    id: paymentId,
    merchant_id: merchantId,
    status: "completed",
    amount: "42.5000000",
    asset: "XLM",
    event,
  });
  db.prepare(
    `INSERT INTO webhook_deliveries
       (id, payment_id, url, payload, event_type, status, attempts,
        last_attempt, created_at, manual_attempts)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0)`,
  ).run(id, paymentId, url, payload, event, status, attempts, now, now);
}

/** Remove anything a previous run left behind, so a re-run starts clean. */
function resetFixtures() {
  if (!existsSync(DB_FILE)) {
    throw new Error(
      `the database file ${DB_FILE} does not exist; is the server running?`,
    );
  }
  const db = new DatabaseSync(DB_FILE);
  db.exec("DELETE FROM webhook_deliveries");
  db.exec("DELETE FROM processed_transactions");
  db.exec("DELETE FROM idempotency_keys");
  /* Scoped to this suite's memo prefix so a hand-created payment in a shared
     development database is not silently destroyed. */
  db.exec("DELETE FROM payments WHERE memo LIKE 'e2e-order-%'");
  db.close();
}

export async function seed() {
  await waitForServer();

  /* A previous run's rows would collide on the fixed delivery ids below and,
     worse, leave stale payments in the list so the expected row counts drift.
     The database file belongs to the suite, not to a merchant, so clearing it
     is correct — but only the tables this seed populates, so a partially
     seeded run cannot be mistaken for a clean one. */
  resetFixtures();

  /* The receiver has to exist before the payments are created, because the
     gateway validates a webhook_url by resolving it at creation time. */
  const receiver = await startReceiver();

  // 1. Provision a merchant through the admin-gated endpoint.
  const merchant = await api("/merchants", {
    method: "POST",
    admin: ADMIN_SECRET,
  });
  const merchantId = merchant.merchant_id;
  const apiKey = merchant.api_key;

  if (!merchantId || !apiKey) {
    throw new Error("merchant provisioning returned no merchant_id / api_key");
  }

  /* 2. Create the payment intents through the public API.

     The list is ordered by `created_at DESC, id DESC`, so the payments are
     created back-to-back and given increasing `created_at` values. That fixes
     the row order the specs assert against; without it the list's order is
     whatever the clock and the random UUIDs happened to produce, and a spec
     that assumed "the first row is the one I seeded" would be flaky rather
     than wrong. */
  const specs = [
    { amount: "42.5", memo: "e2e-order-alpha", webhookUrl: receiver.url },
    { amount: "10", memo: "e2e-order-bravo" },
    { amount: "99.99", memo: "e2e-order-charlie" },
    { amount: "5", memo: "e2e-order-delta", webhookUrl: receiver.url },
  ];
  const created = [];
  for (const spec of specs) {
    created.push(await createPayment(apiKey, spec));
  }
  const [alpha, bravo, charlie, delta] = created;

  /* The payment objects the API returned still carry the gateway's generated
     memo, which is not what the list shows after this seed rewrites it below.
     Re-stamping the readable label here keeps the fixture and the rendered
     row in agreement — otherwise a spec matching on `memo` silently tests
     against a value the dashboard never displays. */
  const labelled = created.map((p, i) => ({ ...p, memo: specs[i].memo }));
  const [lAlpha, lBravo, lCharlie, lDelta] = labelled;

  /* Published in the order the list renders them — `created_at DESC`, i.e.
     newest first — so a spec that indexes by position can say which payment it
     means. The timestamps are assigned along this list below, so `rowOrder[i]`
     is both the i-th row on screen and the i-th newest payment. */
  const rowOrder = [lAlpha, lBravo, lCharlie, lDelta];

  // 3. Settle some of them directly, so the filters and status pills have
  //    something to discriminate between.
  const db = new DatabaseSync(DB_FILE);

  // The gateway generated its own memos; replace them with readable labels so a
  // failing spec names the payment it meant.
  [alpha, bravo, charlie, delta].forEach((p, i) => {
    setMemo(db, p.id, specs[i].memo);
  });

  /* Stagger `created_at` down the list order, so `created_at DESC` puts
     rowOrder[0] first by construction. Deriving the order from the list query
     rather than asserting it keeps this correct if the ordering ever changes. */
  const newestFirst = [...rowOrder].reverse(); // oldest first, for ascending times
  const base = Date.now() - newestFirst.length * 60_000;
  newestFirst.forEach((p, i) => {
    const at = new Date(base + i * 60_000).toISOString().replace(/\.\d+Z$/, "Z");
    setCreatedAt(db, p.id, at);
  });

  setStatus(db, alpha.id, "completed", {
    paid_amount: "42.5",
    tx_hash: "e2e-tx-alpha-0000000000000000000000000000",
  });
  setStatus(db, bravo.id, "underpaid", { paid_amount: "4" });
  setStatus(db, charlie.id, "expired", { paid_amount: null });
  // `delta` stays `pending`, which is what the expiry countdown and the
  // auto-refresh interval both care about.

  // 4. Give the completed payment a failed delivery to redeliver, and the
  //    pending one a delivered one so the drawer shows both pill styles.
  insertDelivery(db, {
    id: "e2e-delivery-failed-1",
    paymentId: alpha.id,
    merchantId,
    event: "payment.completed",
    url: receiver.failingUrl,
    status: "failed",
    attempts: 3,
  });
  insertDelivery(db, {
    id: "e2e-delivery-delivered-1",
    paymentId: delta.id,
    merchantId,
    event: "payment.created",
    url: receiver.url,
    status: "delivered",
    attempts: 1,
  });
  db.close();

  // 5. Publish the fixture. The key goes to a git-ignored file, never to stdout.
  const seedData = {
    merchantId,
    apiKey,
    baseUrl: BASE_URL,
    receiver,
    payments: {
      completed: lAlpha,
      underpaid: lBravo,
      expired: lCharlie,
      pending: lDelta,
    },
    /* Newest first — the order `GET /payments` returns, and therefore the order
       the dashboard's rows appear in. Lets a spec that positions by index say
       which payment it means. */
    rowOrder,
    delivery: {
      failed: { id: "e2e-delivery-failed-1", paymentId: alpha.id },
      delivered: { id: "e2e-delivery-delivered-1", paymentId: delta.id },
    },
  };

  writeFileSync(SEED_FILE, JSON.stringify(
    { ...seedData, receiver: { url: receiver.url, failingUrl: receiver.failingUrl } },
    null,
    2,
  ), { mode: 0o600 });

  return seedData;
}

/* Run directly (`npm run seed`) as well as being imported by the specs. */
if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  seed()
    .then((data) => {
      /* Reports the merchant id and the payment count only. The key is in the
         fixture file; printing it here would put it in CI logs, which is the
         exact failure #726 guards against. */
      console.log(
        `seeded merchant ${data.merchantId} with ` +
          `${Object.keys(data.payments).length} payments; ` +
          `webhook receiver on ${data.receiver.url}; ` +
          `credentials written to e2e/.seed.json`,
      );
      /* The receiver stays listening for as long as this process does, so a
         hand-run (`npm run seed` then poke at the gateway) can watch deliveries
         arrive. The specs do not rely on that: they read `receiver` off the
         returned object, which keeps them alive for the worker's lifetime. */
    })
    .catch((e) => {
      console.error(`seed failed: ${e.message}`);
      process.exit(1);
    });
}
