# Webhook Reference Guide

This is the **canonical webhook documentation** for StellarGate. For webhook delivery management endpoints (list/redeliver), see [Webhook Delivery Management](#webhook-delivery-management). For integration examples, see [Integration Examples](#integration-examples).

## Overview

When a payment reaches a terminal state, StellarGate POSTs a signed JSON event to your webhook endpoint. Every request carries cryptographic headers that let you verify both authenticity and freshness — preventing replay attacks and tampering.

```
Payment created (pending)
    ↓
On-chain transaction detected
    ↓
Amount reconciled (completed/overpaid/underpaid)
    ↓
Webhook dispatched with signed payload
```

## Payload detail

`WEBHOOK_PAYLOAD_DETAIL` controls how much of the payload below you actually
receive, and **defaults to `minimal`, not `full`**. Every example in this
document shows the `full` payload (`WEBHOOK_PAYLOAD_DETAIL=full`) — the
previously-default, richer shape — for completeness; under the default
`minimal` setting, only `event`, `payment_id`, `status`, and `updated_at` are
sent. `merchant_id`, `tx_hash`, `amount`, `paid_amount`, `asset`,
`asset_issuer`, and `delta` are `full`-only. See the README's "Webhooks" →
"Payload detail" section and [SECURITY.md](SECURITY.md#webhook-payload-exposure)
for why: HMAC signing proves authenticity, not confidentiality, and this
service cannot guarantee your endpoint is reached over an encrypted
connection on every network. If your integration needs the omitted fields,
either set `WEBHOOK_PAYLOAD_DETAIL=full` or call `GET /v1/payments/:id` with
your API key.

## Event identity and deduplication

Every webhook body carries a stable, unique event identifier in the `id`
field, plus an RFC 3339 `created_at` timestamp. Both are part of the signed
body, so they are covered by the HMAC signature.

- `id` is unique per event. It is **identical across inline retries, redrive,
  and manual redelivery** of the same event, because those replay the stored
  payload verbatim.
- **Deduplicate on `id`.** If you receive the same `id` twice, it is a retry
  of the same event and you should ignore the duplicate.
- **Do not deduplicate on `payment_id`.** A single payment legitimately emits
  several different events (for example `payment.underpaid` followed by
  `payment.completed` after a top-up, or `payment.completed` followed by
  `payment.unexpected` when the payer sends again). Deduplicating on
  `payment_id` would drop those real follow-up events. Use `payment_id` only
  to correlate events that belong to the same payment.
- Deduplicating on `(payment_id, event)` is also unsafe: `payment.unexpected`
  and `payment.underpaid` can each fire more than once for the same payment
  with different amounts.

## Event Types

StellarGate fires exactly one event when a payment settles, determined by comparing received amount to requested amount:

### `webhook.test`

Sent only by `POST /v1/webhooks/test`, so a merchant can check a receiver, its TLS
setup and its signature verification without paying on-chain. The payload is
`{ "event": "webhook.test", "sentAt": <unix seconds> }`. It is signed and sent with the same
headers as real events. It is not stored in webhook deliveries, so it does not appear in
`GET /v1/payments/{id}/webhooks`.

### `payment.completed`

Fired when cumulative payment equals the requested amount exactly.

```json
{
  "id": "9f1c2e7a-4b3d-4e5f-8a6b-1c2d3e4f5a6b",
  "created_at": "2024-06-22T15:30:45Z",
  "event": "payment.completed",
  "payment_id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "tx_hash": "abc123def456...",
  "amount": "10.00",
  "paid_amount": "10.00",
  "asset": "XLM",
  "asset_issuer": null,
  "status": "completed"
}
```

### `payment.overpaid`

Fired when cumulative payment **exceeds** the requested amount. The `delta` field shows the excess amount to consider refunding.

```json
{
  "id": "9f1c2e7a-4b3d-4e5f-8a6b-1c2d3e4f5a6b",
  "created_at": "2024-06-22T15:30:45Z",
  "event": "payment.overpaid",
  "payment_id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "tx_hash": "abc123def456...",
  "amount": "10.00",
  "paid_amount": "12.50",
  "asset": "XLM",
  "status": "completed",
  "delta": "2.50"
}
```

### `payment.underpaid`

Fired when a payment arrives but falls **short** of the requested amount. The `delta` field shows the remaining shortfall. The intent remains open for a top-up payment.

```json
{
  "id": "9f1c2e7a-4b3d-4e5f-8a6b-1c2d3e4f5a6b",
  "created_at": "2024-06-22T15:30:45Z",
  "event": "payment.underpaid",
  "payment_id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "tx_hash": "abc123def456...",
  "amount": "10.00",
  "paid_amount": "7.00",
  "asset": "XLM",
  "status": "underpaid",
  "delta": "3.00"
}
```

### `payment.expired`

Fired when a payment intent's TTL elapses before payment arrives. No further transactions are watched for this intent.

```json
{
  "id": "9f1c2e7a-4b3d-4e5f-8a6b-1c2d3e4f5a6b",
  "created_at": "2024-06-22T15:30:45Z",
  "event": "payment.expired",
  "payment_id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "tx_hash": null,
  "amount": "10.00",
  "paid_amount": null,
  "asset": "XLM",
  "status": "expired"
}
```

### `payment.unexpected`

Fired when an on-chain payment arrives for an intent that is **already terminal** — the customer paid again after the intent reached `completed`, or paid after it `expired`. Once an intent is terminal the gateway never changes its status, and it is the only component that can see such a payment, so this event is the merchant's signal that funds have arrived that must be returned. **It always requires the merchant to act.**

- `delta` is the **unexpected amount** — the amount that just arrived and must be refunded.
- `status` is **unchanged**: it keeps the intent's terminal value (`completed` or `expired`). The intent's own `amount` and `paid_amount` are not updated by the new payment.
- **What to do with the funds:** refund `delta` to the payer. The gateway records the transaction so it is not re-processed, but it does not move the funds for you.

```json
{
  "id": "9f1c2e7a-4b3d-4e5f-8a6b-1c2d3e4f5a6b",
  "created_at": "2024-06-22T15:30:45Z",
  "event": "payment.unexpected",
  "payment_id": "a1b2c3d4-...",
  "merchant_id": "your-merchant-id",
  "tx_hash": "abc123def456...",
  "amount": "10.00",
  "paid_amount": "10.00",
  "asset": "XLM",
  "asset_issuer": null,
  "status": "completed",
  "delta": "5.00"
}
```

> `payment.unexpected` can fire more than once for the same payment with
> different amounts (e.g. a customer sends again, then again). Deduplicate on
> the `id` field, never on `(payment_id, event)` — see
> [Event identity and deduplication](#event-identity-and-deduplication).

## Webhook Headers

Every webhook request includes four headers:

| Header | Description | Signed? |
|---|---|---|
| `X-StellarGate-Timestamp` | Unix time (seconds) when event was signed | ✅ Yes |
| `X-StellarGate-Signature` | Hex HMAC-SHA256 of `"{timestamp}.{raw_body}"` | ✅ Yes |
| `X-StellarGate-Event` | Copy of the `event` field from body (routing convenience) | ❌ No |
| `X-StellarGate-Delivery` | Copy of the `id` field from body (deduplication convenience) | ❌ No |

**Important:** `X-StellarGate-Event` and `X-StellarGate-Delivery` are not covered by the HMAC signature. They mirror the body's `event` and `id` fields but can be altered in transit. Always read the event type and event id from the signed JSON body after verifying the signature.

**Retries are re-signed.** Each inline delivery attempt (including every retry after a backoff) is signed at send time with a fresh `X-StellarGate-Timestamp` and a matching `X-StellarGate-Signature`. Redrive and manual redelivery behave the same way. This means the timestamp always reflects when that particular attempt was sent, not when the event was first generated — so a retry that fires minutes after the original attempt still passes the freshness check below. The `id` in the body stays constant across attempts (see [Event identity and deduplication](#event-identity-and-deduplication)); only the signature headers change.

## Verifying Webhooks

Use this recipe to verify each incoming webhook:

1. **Extract headers:**
   - Read `X-StellarGate-Timestamp` as `t` (Unix seconds)
   - Read `X-StellarGate-Signature` as `sig` (hex string)

2. **Check timestamp freshness:**
   - Reject if `abs(now - t) > tolerance`
   - Recommended tolerance: **5 minutes** (300 seconds)
   - This bounds the replay window: a stolen request becomes useless after 5 minutes
   - Because retries are re-signed at send time, a retry is never rejected for being stale regardless of how long the backoff was

3. **Recompute signature:**
   - Get the **exact raw bytes** received (before JSON re-encoding)
   - Compute `HMAC_SHA256(WEBHOOK_SECRET, "{t}.{raw_body}")`
   - Hex-encode the result
   - Example: if body is `{"event":"payment.completed"...}` and `t` is `1719072645`, compute HMAC over the string `"1719072645.{\"event\":\"payment.completed\"...}"`

4. **Constant-time comparison:**
   - Compare computed signature to `sig` using a **timing-safe** equality check
   - Reject on mismatch

5. **Parse and route:**
   - After signature verification passes, parse the JSON
   - Read the `event` field from the body to determine the event type
   - Read the `id` field from the body and deduplicate on it
   - Route based on `event` (not on the `X-StellarGate-Event` header)

### Verification Examples

**Node.js:**

```javascript
const crypto = require("crypto");

function verify(rawBody, headers, secret, toleranceSec = 300) {
  const t = Number(headers["x-stellargate-timestamp"]);
  const sig = headers["x-stellargate-signature"];
  
  // 1. Check timestamp freshness
  if (!Number.isFinite(t) || Math.abs(Date.now() / 1000 - t) > toleranceSec) {
    return false; // stale or missing timestamp
  }
  
  // 2. Recompute signature
  const expected = crypto
    .createHmac("sha256", secret)
    .update(`${t}.${rawBody}`)
    .digest("hex");
  
  // 3. Constant-time comparison
  return crypto.timingSafeEqual(Buffer.from(sig), Buffer.from(expected));
}

// Usage: always read the event type from the verified body
function handleWebhook(rawBody, headers, secret) {
  if (!verify(rawBody, headers, secret)) {
    throw new Error("invalid signature");
  }
  
  const payload = JSON.parse(rawBody);
  const event = payload.event; // authenticated; safe to route on
  const eventId = payload.id;  // authenticated; deduplicate on this
  
  switch (event) {
    case "payment.completed":
      console.log("Payment completed:", payload.payment_id);
