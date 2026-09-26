//! Dashboard asset checks.
//!
//! The static checks below inspect the compiled-in JS directly; the router
//! checks serve each asset through the real `api::router` so a response-builder
//! or header API change (e.g. the axum 0.7 → 0.8 upgrade, #637) that drops a
//! content type or the Content-Security-Policy fails CI.

use axum_test::TestServer;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::sync::Arc;
use stellargate::{
    AppState, api,
    config::{Config, ListenerMode},
    db,
};

fn make_config() -> Config {
    Config {
        port: 0,
        database_url: "sqlite::memory:".into(),
        network: "testnet".into(),
        horizon_url: String::new(),
        gateway_public: "UNCONFIGURED".into(),
        accepted_assets: stellargate::config::AcceptedAsset::default_list(),
        webhook_secret: String::new(),
        webhook_retry_attempts: 1,
        webhook_retry_delay_ms: 0,
        webhook_retry_max_delay_ms: 60_000,
        allowed_webhook_schemes: vec!["https".into(), "http".into()],
        webhook_timeout_secs: 10,
        webhook_redrive_interval_secs: 30,
        webhook_redrive_concurrency: 4,
        webhook_redrive_max_attempts: 8,
        webhook_redrive_grace_secs: 60,
        webhook_redrive_backoff_initial_secs: 0,
        webhook_redrive_backoff_max_secs: 0,
        retention_interval_secs: 3600,
        webhook_delivery_retention_days: 30,
        idempotency_retention_days: 7,
        poll_interval_secs: 10,
        poll_max_pages_per_cycle: 50,
        payment_ttl_secs: 3600,
        rate_limit_requests_per_sec: 1000,
        db_pool_max_connections: 10,
        db_busy_timeout_ms: 5000,
        cors_allowed_origins: vec![],
        listener_mode: ListenerMode::Poll,
        webhook_allow_private_targets: false,
        admin_provisioning_secret: TEST_ADMIN_SECRET.into(),
        metrics_token: String::new(),
        request_timeout_secs: 30,
        stream_idle_timeout_secs: 30,
        trusted_proxy_cidrs: vec![],
    }
}

const DASHBOARD_HTML: &str = include_str!("../static/dashboard.html");
const DASHBOARD_CSS: &str = include_str!("../static/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../static/dashboard.js");
const DASHBOARD_FORMAT_JS: &str = include_str!("../static/format.js");
const DASHBOARD_SESSION_JS: &str = include_str!("../static/session.js");
const DASHBOARD_STATE_JS: &str = include_str!("../static/state.js");
const DASHBOARD_KEYS_JS: &str = include_str!("../static/keys.js");

/// Every dashboard module, its served path, and its source file. Kept as one
/// table so a new module cannot be added to the router without also being added
/// to the serving, content-type and header assertions below.
const MODULES: &[(&str, &str)] = &[
    ("/dashboard/format.js", DASHBOARD_FORMAT_JS),
    ("/dashboard/session.js", DASHBOARD_SESSION_JS),
    ("/dashboard/state.js", DASHBOARD_STATE_JS),
    ("/dashboard/keys.js", DASHBOARD_KEYS_JS),
];

/// Every JS asset route, entry point included.
const JS_ROUTES: &[&str] = &[
    "/dashboard/app.js",
    "/dashboard/format.js",
    "/dashboard/session.js",
    "/dashboard/state.js",
    "/dashboard/keys.js",
];

#[test]
fn dashboard_api_requests_use_canonical_v1_base() {
    assert!(
        DASHBOARD_JS.contains(r#"var API_BASE = "/v1";"#),
        "the dashboard must define the canonical API version once"
    );
    assert!(
        DASHBOARD_JS.contains("return fetch(API_BASE + path,"),
        "the authenticated API helper must prefix every request with API_BASE"
    );

    let direct_fetches: Vec<_> = DASHBOARD_JS
        .lines()
        .filter(|line| line.contains("fetch("))
        .map(str::trim)
        .collect();
    assert_eq!(
        direct_fetches,
        [
            "return fetch(API_BASE + path, { method: opts.method || \"GET\", headers: headers }).then(",
            "fetch(\"/\")",
            "fetch(\"/ready\", { headers: { Accept: \"application/json\" } })",
        ],
        "new dashboard fetches must use the versioned API helper unless they target an explicitly unversioned operational endpoint"
    );

    assert_eq!(
        DASHBOARD_JS.matches("/v1").count(),
        1,
        "API_BASE must be the only /v1 literal so requests cannot become /v1/v1/..."
    );
}

/// The dashboard must be loaded as an ES module so its `import` statements
/// resolve, and every module must be reached by a *relative* specifier.
///
/// The relative form is load-bearing, not a style choice (#723). An absolute
/// `/dashboard/format.js` resolves in the browser and then fails under
/// `node --test`, because there is no such path on the test runner's
/// filesystem. The unit tests for these modules can only exist while the import
/// is relative, so this assertion is what keeps the two from drifting apart.
#[test]
fn dashboard_modules_are_loaded_as_relative_es_module_imports() {
    assert!(
        DASHBOARD_HTML.contains(r#"<script type="module" src="/dashboard/app.js">"#),
        "dashboard.html must load app.js as a module script so ES import statements resolve"
    );

    assert!(
        !relative_imports(DASHBOARD_JS).is_empty(),
        "dashboard.js must import its sibling modules"
    );

    for (path, body) in
        std::iter::once(("/dashboard/app.js", DASHBOARD_JS)).chain(MODULES.iter().copied())
    {
        for line in body
            .lines()
            .filter(|l| l.trim_start().starts_with("import "))
        {
            assert!(
                !line.contains("\"/dashboard/"),
                "{path} must import its siblings relatively (found: `{}`). An absolute \
                 specifier resolves in the browser but not under `node --test`, which \
                 is precisely what makes these modules unit-testable (#723).",
                line.trim()
            );
        }
    }
}

/// The format module must still export the helpers the other modules rely on.
#[test]
fn format_module_exports_required_helpers() {
    for (path, body) in MODULES {
        if path.ends_with("format.js") {
            assert!(
                body.contains("export function fmtTime"),
                "{path} must export fmtTime"
            );
            assert!(
                body.contains("export function shortId"),
                "{path} must export shortId"
            );
        }
    }
}

/// Every relative `import` in a dashboard module must resolve to a route the
/// router actually serves, and to a file that exists.
///
/// Without this, splitting the dashboard into modules (issue #723) would let a
/// typo ship as a 404 that only shows up as a blank page in the browser — the
/// entry module fails to evaluate, and the sign-in form the HTML ships with is
/// the only thing the operator sees. The check runs over the router, so it also
/// catches a module that exists on disk but was never routed.
#[tokio::test]
async fn every_dashboard_module_import_resolves_to_a_served_route() {
    let server = test_server().await;

    for (source_path, body) in
        std::iter::once(("/dashboard/app.js", DASHBOARD_JS)).chain(MODULES.iter().copied())
    {
        for specifier in relative_imports(body) {
            let resolved = format!("/dashboard/{specifier}");
            assert!(
                JS_ROUTES.contains(&resolved.as_str()),
                "{source_path} imports {specifier:?}, which resolves to {resolved} — \
                 no such route. Add it to the router in src/api/mod.rs."
            );

            let res = server.get(&resolved).await;
            res.assert_status_ok();
            assert_eq!(
                res.header("content-type"),
                "text/javascript; charset=utf-8",
                "{resolved} must be served as a JavaScript module"
            );
        }
    }
}

/// Extract the specifiers of every relative `import ... from "x"` statement.
fn relative_imports(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in source.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("import ") else {
            continue;
        };
        // Only the single-line `import { … } from "x";` form is used; a bare
        // `import "x";` is handled too so a side-effect import is not missed.
        let Some(from) = rest.rsplit_once(" from ").map(|(_, tail)| tail) else {
            continue;
        };
        let Some(quoted) = from
            .trim()
            .strip_prefix('"')
            .and_then(|s| s.split_once('"'))
            .map(|(spec, _)| spec)
        else {
            continue;
        };
        if quoted.starts_with("./") {
            out.push(quoted.trim_start_matches("./").to_string());
        }
    }
    out
}

/// The API key must travel in the `Authorization` header and nowhere else
/// (issue #726).
///
/// This is the static half of the guarantee: it fails at compile time if a
/// future change starts interpolating the key into a URL, appending it to a
/// query string, or logging it. The matching end-to-end assertion over real
/// browser traffic lives in the Playwright suite, which is the only place that
/// can catch a leak built at runtime rather than in source.
#[test]
fn dashboard_never_puts_the_api_key_in_a_url_or_the_console() {
    // The only place the key is read is the header assignment.
    assert_eq!(
        DASHBOARD_JS
            .matches(r#"headers.Authorization = "Bearer " + key"#)
            .count(),
        1,
        "the key must be attached to exactly one place: the Authorization header"
    );

    for (name, source) in
        std::iter::once(("dashboard.js", DASHBOARD_JS)).chain(MODULES.iter().map(|(p, b)| (*p, *b)))
    {
        // Any interpolation of a credential-looking variable into a string that
        // reaches a URL, the hash, or the console is the leak we care about.
        for forbidden in [
            "api_key=",
            "apikey=",
            "access_token=",
            "?key=",
            "console.log",
            "console.debug",
            "console.info",
            "console.warn",
            "console.error",
        ] {
            assert!(
                !source.contains(forbidden),
                "{name} must not contain {forbidden:?}: the API key belongs in the \
                 Authorization header only, never in a URL, the hash, or a log line"
            );
        }

        // `location` writes are how a secret ends up in browser history and in
        // any screenshot of the address bar. The hash carries filters only, and
        // `static/state.js` enforces that with an explicit key allow-list.
        for forbidden in ["location.href =", "location.search", "location.pathname ="] {
            if name.ends_with("app.js") {
                assert!(
                    !source.contains(forbidden),
                    "dashboard.js must not assign {forbidden:?}: a credential in the \
                     address bar persists in history and leaks via the referrer"
                );
            }
        }
    }
}

/// Filter state round-trips through the URL hash, and the hash carries nothing
/// else — it is the one part of the URL an operator is expected to share.
#[test]
fn dashboard_hash_state_only_ever_carries_filters() {
    assert!(
        DASHBOARD_STATE_JS.contains(r#"const HASH_KEYS = ["status", "search", "auto_refresh"];"#),
        "the hash allow-list must stay explicit: serialising a state object \
         wholesale is how a credential ends up in a shareable URL"
    );
}

const TEST_ADMIN_SECRET: &str = "test-admin-secret";

async fn test_server() -> TestServer {
    let cfg = make_config();
    let pool = SqlitePoolOptions::new()
        .connect_with(
            SqliteConnectOptions::from_str(&cfg.database_url)
                .unwrap()
                .create_if_missing(true),
        )
        .await
        .unwrap();
    db::migrate(&pool).await.unwrap();
    let router = api::router(Arc::new(AppState {
        pool,
        config: cfg,
        http: reqwest::Client::new(),
        webhook_http: reqwest::Client::new(),
        webhook_metrics: stellargate::metrics::WebhookMetrics::new(),
        auth_metrics: stellargate::metrics::AuthMetrics::new(),
        horizon_metrics: stellargate::metrics::HorizonMetrics::new(),
        trustline_metrics: stellargate::metrics::TrustlineMetrics::new(),
        http_metrics: stellargate::metrics::HttpMetrics::new(),
        payment_metrics: stellargate::metrics::PaymentMetrics::new(),
        task_health: stellargate::TaskHealth::new(),
    }))
    .into_make_service_with_connect_info::<std::net::SocketAddr>();
    TestServer::new(router)
}

/// Mirrors `DASHBOARD_CSP` in `src/api/mod.rs`. Compared verbatim so any
/// change to the policy — or the header being dropped by a framework upgrade —
/// is a deliberate, reviewed edit rather than a silent regression.
const EXPECTED_CSP: &str = "default-src 'none'; \
     script-src 'self'; \
     style-src 'self'; \
     img-src 'self' data:; \
     connect-src 'self'; \
     form-action 'none'; \
     frame-ancestors 'none'; \
     base-uri 'none'";

/// Every dashboard asset must be served with its exact content type, the full
/// CSP, and the baseline security headers the outer layers add (#637).
#[tokio::test]
async fn dashboard_assets_keep_content_type_and_csp() {
    let server = test_server().await;

    let mut assets: Vec<(&str, &str, &str)> = vec![
        ("/dashboard", "text/html; charset=utf-8", DASHBOARD_HTML),
        (
            "/dashboard/app.css",
            "text/css; charset=utf-8",
            DASHBOARD_CSS,
        ),
        (
            "/dashboard/app.js",
            "text/javascript; charset=utf-8",
            DASHBOARD_JS,
        ),
    ];
    assets.extend(
        MODULES
            .iter()
            .map(|(path, body)| (*path, "text/javascript; charset=utf-8", *body)),
    );

    for (path, content_type, body) in assets {
        let res = server.get(path).await;
        res.assert_status_ok();
        assert_eq!(
            res.header("content-type"),
            content_type,
            "{path} served with the wrong content type"
        );
        assert_eq!(
            res.header("content-security-policy"),
            EXPECTED_CSP,
            "{path} must carry the dashboard CSP unchanged"
        );
        assert_eq!(res.header("x-content-type-options"), "nosniff", "{path}");
        assert_eq!(res.header("referrer-policy"), "no-referrer", "{path}");
        assert_eq!(res.header("cache-control"), "no-store", "{path}");
        assert_eq!(
            res.text(),
            body,
            "{path} must serve the include_str! asset byte-for-byte"
        );
    }
}

/// An unknown file under `/dashboard/` must 404 rather than being resolved from
/// disk. The modules are individually routed from `include_str!` constants, so
/// there is no directory to traverse — this pins that property so a future
/// wildcard route cannot quietly expose `static/`.
#[tokio::test]
async fn unknown_dashboard_module_is_not_found() {
    let server = test_server().await;

    for path in [
        "/dashboard/../Cargo.toml",
        "/dashboard/tests/format.test.js",
        "/dashboard/dashboard.js",
        "/dashboard/nope.js",
    ] {
        let res = server.get(path).await;
        res.assert_status_not_found();
    }
}

/// Each header must appear exactly once — a response builder that appends
/// instead of replacing would otherwise emit a duplicate `nosniff` alongside
/// the outer security-header layer's copy.
#[tokio::test]
async fn dashboard_security_headers_are_not_duplicated() {
    let server = test_server().await;

    for path in std::iter::once("/dashboard")
        .chain(std::iter::once("/dashboard/app.css"))
        .chain(JS_ROUTES.iter().copied())
    {
        let res = server.get(path).await;
        res.assert_status_ok();
        for name in [
            "content-type",
            "content-security-policy",
            "x-content-type-options",
        ] {
            assert_eq!(
                res.headers().get_all(name).iter().count(),
                1,
                "{path} must send exactly one {name} header"
            );
        }
    }
}
