#!/usr/bin/env node
/* A local webhook receiver for the end-to-end suite.
 *
 * A redelivery test that only checks the dashboard's own bookkeeping proves
 * very little: the button could report success while nothing reached the
 * merchant. This stands in for the merchant's endpoint so the suite can assert
 * on what actually arrived — the payload, the event, and the signature header.
 *
 * It binds to 127.0.0.1, which the gateway's SSRF guard rejects unless
 * `WEBHOOK_ALLOW_PRIVATE_TARGETS=true`; the Playwright config sets that, and it
 * is rejected outright on the public network, so this cannot leak into a real
 * deployment.
 */

import { createServer } from "node:http";
import { createHmac, timingSafeEqual } from "node:crypto";

/** Bodies received since the last reset, newest last. */
const received = [];

/** Shared secret the gateway signs with. Must match WEBHOOK_SECRET. */
const secret = process.env.WEBHOOK_SECRET || "";

/** Start the receiver on an ephemeral port and return `{ url, received, close }`. */
export async function startReceiver() {
  const server = createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const body = Buffer.concat(chunks).toString("utf8");

      let parsed = null;
      try {
        parsed = JSON.parse(body);
      } catch {
        /* recorded as unparseable below rather than dropped */
      }

      received.push({
        method: req.method,
        path: req.url,
        headers: { ...req.headers },
        body,
        json: parsed,
        signatureValid: verifySignature(req.headers, body),
      });

      /* 200 for everything except a path containing "fail", which lets a spec
         drive a redelivery straight back into the failed state. */
      const shouldFail = (req.url || "").includes("fail");
      res.writeHead(shouldFail ? 500 : 200, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ ok: !shouldFail }));
    });
  });

  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });

  const { port } = server.address();

  return {
    url: `http://127.0.0.1:${port}/webhook`,
    failingUrl: `http://127.0.0.1:${port}/fail`,
    received,
    reset: () => {
      received.length = 0;
    },
    close: () =>
      new Promise((resolve) => {
        server.close(() => resolve());
      }),
  };
}

/**
 * Verify the HMAC-SHA256 signature the gateway attaches to every delivery.
 *
 * The signature is a bare hex digest over `"{timestamp}.{body}"`, with the
 * timestamp carried in its own `X-StellarGate-Timestamp` header — not a
 * comma-delimited `t=…,v1=…` value as in the Stripe-style scheme. Matches
 * `webhook::sign` in src/webhook.rs.
 */
function verifySignature(headers, body) {
  // Node hands headers over as `string | string[]`; a duplicated header is not
  // something this server produces, so anything but a string is a failure
  // rather than something to coerce.
  const signature = headers["x-stellargate-signature"];
  const timestamp = headers["x-stellargate-timestamp"];
  if (typeof signature !== "string" || typeof timestamp !== "string") return false;
  if (!/^[0-9a-f]{64}$/.test(signature)) return false;

  const expected = createHmac("sha256", secret)
    .update(`${timestamp}.${body}`)
    .digest("hex");

  const a = Buffer.from(signature, "hex");
  const b = Buffer.from(expected, "hex");
  if (a.length !== b.length) return false;
  return timingSafeEqual(a, b);
}

if (process.argv[1] && process.argv[1].endsWith("receiver.mjs")) {
  /* Standalone mode, for poking at the suite by hand. */
  const r = await startReceiver();
  console.log(`webhook receiver listening on ${r.url}`);
}
