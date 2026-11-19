//! Locks `openapi.yaml`'s `CreatePaymentRequest` schema to the Rust struct of
//! the same name (`src/api/payments.rs`), so the two cannot silently drift
//! apart again the way they did for `merchant_id` (issue #307): a client
//! generated from the spec would have exposed a settable field the server
//! never read.
//!
//! There's no YAML-parsing dependency in this crate, so this reads
//! `openapi.yaml` as text and extracts the property names declared directly
//! under `CreatePaymentRequest.properties` by indentation. It's intentionally
//! narrow — a full spec-vs-router reflection check is a larger project
//! (tracked separately) — but it means a field added to one side without the
//! other fails `cargo test`, in CI, on every PR.
//!
//! It also checks the router → spec direction for the payments summary
//! endpoint (issue #869): `GET /v1/payments/summary` is mounted in
//! `src/api/mod.rs` but was missing from `openapi.yaml`, and the existing
//! spec → router contract test couldn't notice. The list below mirrors the
//! routes the router serves; every entry must appear as a path in the spec
//! (or be explicitly marked internal).

use std::collections::BTreeSet;

#[test]
fn create_payment_request_spec_matches_struct_fields() {
    // Mirrors `CreatePaymentRequest` in src/api/payments.rs. If that struct's
    // fields change, this list (and the assertion below) must change with it.
    let struct_fields: BTreeSet<&str> = ["amount", "asset", "webhook_url"].into_iter().collect();

    let spec = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.yaml"))
        .expect("openapi.yaml must be readable");

    // Schema keys under `components.schemas` sit at exactly four spaces of
    // indentation (`    CreatePaymentRequest:`, `    ListPaymentsResponse:`,
    // ...). Collect every line from that header up to — but not including —
    // the next line at that same four-space depth, i.e. the next sibling
    // schema.
    let mut lines = spec.lines();
    for line in lines.by_ref() {
        if line == "    CreatePaymentRequest:" {
            break;
        }
    }
    let block: String = lines
        .take_while(|line| {
            !(line.starts_with("    ") && !line.starts_with("     ") && !line.is_empty())
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !block.is_empty(),
        "openapi.yaml must declare a CreatePaymentRequest schema with a non-empty body"
    );
    let block = block.as_str();

    assert!(
        block.contains("additionalProperties: false"),
        "CreatePaymentRequest must set `additionalProperties: false` so an \
         unrecognised field (like a stray merchant_id) is rejected with 400 \
         rather than silently ignored"
    );
    assert!(
        !block.contains("        merchant_id:"),
        "merchant_id must not reappear as a property of CreatePaymentRequest \
         — tenancy is derived from the bearer token, not the request body \
         (issue #307)"
    );

    // Property names are the lines indented exactly eight spaces within the
    // schema block (sub-fields of a property, e.g. `type:`, sit ten+ deep).
    let spec_fields: BTreeSet<&str> = block
        .lines()
        .filter_map(|line| line.strip_prefix("        "))
        .filter(|rest| !rest.starts_with(' ') && !rest.starts_with('#'))
        .filter_map(|rest| rest.split_once(':').map(|(name, _)| name))
        .collect();

    assert_eq!(
        spec_fields, struct_fields,
        "openapi.yaml's CreatePaymentRequest properties must match \
         CreatePaymentRequest's fields in src/api/payments.rs field-for-field \
         — update whichever side is now out of date"
    );
}

/// Every `(method, path)` the router serves, mirroring the `.route(...)` calls
/// in `src/api/mod.rs`. Paths are the OpenAPI-style templates (e.g.
/// `/v1/payments/{id}`), not the axum `:id` form. Entries marked `internal`
/// (dashboard assets, health probes) are exempt from the spec check.
const ROUTER_ROUTES: &[(&str, &str, bool)] = &[
    ("get", "/v1/payments", false),
    ("post", "/v1/payments", false),
    ("get", "/v1/payments/{id}", false),
    ("post", "/v1/payments/{id}/cancel", false),
    ("get", "/v1/payments/summary", false),
    ("get", "/payments", false),
    ("post", "/payments", false),
    ("get", "/payments/{id}", false),
    ("post", "/payments/{id}/cancel", false),
    ("get", "/payments/summary", false),
    ("get", "/health", true),
    ("get", "/dashboard/{*path}", true),
];

#[test]
fn every_router_route_is_documented_in_spec() {
    let spec = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/openapi.yaml"))
        .expect("openapi.yaml must be readable");

    // Path keys under `paths:` sit at exactly two spaces of indentation
    // (`  /v1/payments:`). Collect them so we can look up each router route.
    let documented_paths: BTreeSet<&str> = spec
        .lines()
        .filter_map(|line| line.strip_prefix("  "))
        .filter(|rest| !rest.starts_with(' ') && rest.starts_with('/'))
        .filter_map(|rest| rest.split_once(':').map(|(path, _)| path))
        .collect();

    let mut missing = Vec::new();
    for (method, path, internal) in ROUTER_ROUTES {
        if *internal {
            continue;
        }
        if !documented_paths.contains(path) {
            missing.push(format!("{method} {path}"));
        }
    }

    assert!(
        missing.is_empty(),
        "openapi.yaml is missing paths for router routes: {missing:?} — \
         document them in openapi.yaml (or mark them internal in \
         ROUTER_ROUTES if they are not part of the public API)"
    );
}
