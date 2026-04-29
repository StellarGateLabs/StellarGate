# StellarGate

A developer-friendly payment gateway API built on [Stellar](https://stellar.org) for accepting, verifying, and managing payments in XLM and USDC.

> Think Stripe — but powered by the Stellar blockchain instead of banks.

## Overview

StellarGate abstracts Stellar payments into a simple REST API. Developers can create payment intents, receive a destination address and memo, and get notified when payment is confirmed on-chain.

```
Client App → POST /payments → get address + memo
User pays via Stellar wallet (e.g. Lobstr)
StellarGate detects transaction on Horizon
Payment marked complete → webhook fired to your app
```

## Current Status

This project is under active development. The following is implemented:

- [x] `POST /payments` — create a payment intent
- [x] `GET /payments/:id` — query payment status
- [x] `GET /health` — health check
- [x] SQLite persistence
- [x] Input validation (asset, amount)
- [ ] Transaction listener (Horizon streaming)
- [ ] Payment verification
- [ ] Webhook dispatch
- [ ] List/filter payments
- [ ] Multi-merchant support
- [ ] Dashboard UI

## Tech Stack

- **Language:** Rust
- **HTTP Framework:** [axum](https://github.com/tokio-rs/axum)
- **Database:** SQLite via [sqlx](https://github.com/launchbadge/sqlx)
- **Async Runtime:** [tokio](https://tokio.rs)
- **Blockchain:** [Stellar Horizon API](https://developers.stellar.org/api)

## Getting Started

### Prerequisites

- Rust 1.75+ — [install via rustup](https://rustup.rs)

### Setup

```bash
git clone https://github.com/StellarGateLabs/StellarGate.git
cd StellarGate

cp .env.example .env
# Edit .env with your Stellar keys
```

### Environment Variables

| Variable | Description | Default |
|---|---|---|
| `PORT` | HTTP port | `3000` |
| `DATABASE_URL` | SQLite path | `sqlite:stellargate.db` |
| `STELLAR_HORIZON_URL` | Horizon endpoint | testnet |
| `STELLAR_GATEWAY_PUBLIC` | Your gateway wallet public key | — |
| `STELLAR_GATEWAY_SECRET` | Your gateway wallet secret key | — |
| `USDC_ISSUER` | USDC issuer address | testnet issuer |
| `WEBHOOK_SECRET` | HMAC signing secret for webhooks | — |

### Run

```bash
cargo run
```

### Test

```bash
cargo test
```

All 6 tests should pass.

## API Reference

### `POST /payments`

Create a new payment intent.

**Request**
```json
{
  "amount": "10.00",
  "asset": "XLM",
  "merchant_id": "your-merchant-id",
  "webhook_url": "https://yourapp.com/webhooks/stellar"
}
```

| Field | Type | Required | Values |
|---|---|---|---|
| `amount` | string | ✅ | Any positive number |
| `asset` | string | ✅ | `XLM` or `USDC` |
| `merchant_id` | string | ❌ | Any string |
| `webhook_url` | string | ❌ | Valid HTTPS URL |

**Response** `201 Created`
```json
{
  "id": "a1b2c3d4-...",
  "destination_address": "GBBD47IF6LWK7P7...",
  "memo": "A1B2C3D4",
  "amount": "10.00",
  "asset": "XLM",
  "status": "pending",
  "created_at": "2026-04-29T15:00:00"
}
```

> The user must send exactly `amount` of `asset` to `destination_address` with `memo` set as the transaction memo.

---

### `GET /payments/:id`

Fetch the current status of a payment.

**Response** `200 OK`
```json
{
  "id": "a1b2c3d4-...",
  "destination_address": "GBBD47IF6LWK7P7...",
  "memo": "A1B2C3D4",
  "amount": "10.00",
  "asset": "XLM",
  "status": "pending",
  "tx_hash": null,
  "paid_amount": null,
  "created_at": "2026-04-29T15:00:00"
}
```

**Status values**

| Status | Meaning |
|---|---|
| `pending` | Awaiting payment |
| `completed` | Payment confirmed on-chain |
| `failed` | Partial payment or verification failed |

---

### `GET /health`

```
200 OK — "ok"
```

## Payment Flow

```
1. Developer calls POST /payments
2. StellarGate returns { destination_address, memo, amount }
3. End user sends payment via any Stellar wallet
4. StellarGate listener detects the transaction on Horizon
5. Verifies: correct memo + amount + asset
6. Updates payment status to "completed"
7. POSTs webhook event to developer's webhook_url
```

## Webhook Events *(coming soon)*

```json
{
  "event": "payment.success",
  "payment_id": "a1b2c3d4-...",
  "tx_hash": "abc123...",
  "amount": "10.00",
  "paid_amount": "10.00",
  "asset": "XLM"
}
```

Webhooks are signed with `X-StellarGate-Signature` (HMAC-SHA256) so you can verify authenticity.

## Project Structure

```
src/
├── main.rs          # Entry point, server startup
├── lib.rs           # Shared state and module exports
├── config.rs        # Environment configuration
├── db.rs            # Database queries (SQLite)
└── api/
    ├── mod.rs       # Axum router
    └── payments.rs  # Payment handlers

tests/
└── api_tests.rs     # Integration tests
```

## Contributing

This project is open to contributors. See the [Wave Program](https://github.com/StellarGateLabs/StellarGate/issues) for scoped issues you can pick up.

**To contribute:**

1. Fork the repo
2. Create a branch: `git checkout -b feat/your-feature`
3. Make your changes and add tests
4. Run `cargo test` — all tests must pass
5. Open a pull request

## License

MIT
