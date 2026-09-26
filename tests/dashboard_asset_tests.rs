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
const DASHBOARD_THEME_JS: &str = include_str!("../static/dashboard-theme.js");

/// The document with its `<!-- … -->` comments removed.
///
/// The dashboard's markup is heavily commented, and those comments name the
/// very attributes the assertions below look for — counting them without
/// stripping would read a comment as an attribute.
fn strip_html_comments(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(open) = rest.find("<!--") {
        out.push_str(&rest[..open]);
        rest = match rest[open..].find("-->") {
            Some(close) => &rest[open + close + 3..],
            // Unterminated comment: nothing after it can be trusted as markup.
            None => return out,
        };
    }
    out.push_str(rest);
    out
}

const DASHBOARD_FORMAT_JS: &str = include_str!("../static/dashboard-format.js");

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

/// The format module must be loaded as an ES module import (type="module")
/// with the correct path so the browser resolves it against the same origin.
#[test]
fn dashboard_imports_format_module() {
    assert!(
        DASHBOARD_JS.contains(r#"import { fmtTime, shortId } from "/dashboard/format.js";"#),
        "dashboard.js must import fmtTime and shortId from /dashboard/format.js"
    );
    assert!(
        DASHBOARD_HTML.contains(r#"<script type="module" src="/dashboard/app.js">"#),
        "dashboard.html must load app.js as a module script so ES import statements resolve"
    );
}

/// The format module must export the two helpers the main script depends on.
#[test]
fn format_module_exports_required_helpers() {
    assert!(
        DASHBOARD_FORMAT_JS.contains("export function fmtTime"),
        "dashboard-format.js must export fmtTime"
    );
    assert!(
        DASHBOARD_FORMAT_JS.contains("export function shortId"),
        "dashboard-format.js must export shortId"
    );
}

/// Every helper the dashboard calls must actually be defined.
///
/// A file built from stacked PRs has a history of calling helpers that were
/// never written: `formatAmount`, `countdown`, `explorerTx`, `readHashState`,
/// `writeHashState` and `relativeTime` were all invoked but undefined, and the
/// first of them runs during sign-in, so the symptom was a page that never left
/// the sign-in form rather than an error anyone could see. `node --check`
/// cannot catch it — the file is syntactically perfect.
///
/// A full scope analysis would need a JavaScript parser, which this crate
/// deliberately has no route to, so this pins the names the script depends on.
/// Adding a helper to the script means adding it here.
#[test]
fn every_dashboard_helper_the_script_calls_is_defined() {
    for helper in [
        // Amount / time rendering. `formatAmount` and `countdown` are called
        // from the table and the detail fields; `relativeTime` from every
        // delivery row.
        "formatAmount",
        "countdown",
        "relativeTime",
        // Explorer links (#701).
        "explorerTx",
        // View state in the URL hash (#696).
        "readHashState",
        "writeHashState",
        // UI plumbing.
        "syncFilterUi",
        // Modal open/close for the detail drawer (#715).
        "openModal",
        "closeModal",
        "dismissOnBackdrop",
        "onDetailClosed",
        // Theme override (#713).
        "initTheme",
        "applyTheme",
        "effectiveTheme",
        "syncThemeUi",
        // Focus management in the detail drawer (#714).
        "focusDetail",
        "trapDetailFocus",
        "keepFocusInDetail",
    ] {
        assert!(
            DASHBOARD_JS.contains(&format!("function {helper}(")),
            "static/dashboard.js calls `{helper}` but never defines it — every call \
             site is a runtime ReferenceError, and a syntax check will not see it"
        );
    }
}

/// The detail drawer is a real modal dialog (issue #715).
///
/// An `<aside>` under a scrim is only *visually* on top: `Tab`, the address bar
/// and the accessibility tree all still reach the page behind it, because
/// nothing marks that content as inert. `showModal()` is what puts the dialog
/// in the top layer and makes the rest of the page unreachable, so the element
/// choice and the way it is opened have to stay in step — an un-hidden
/// `<dialog>` is no better than the `<aside>` it replaced.
#[test]
fn detail_drawer_is_a_modal_dialog() {
    assert!(
        DASHBOARD_HTML.contains("<dialog"),
        "the drawer must be a <dialog> element; an <aside> cannot be modal, so the \
         page behind it stays reachable by Tab and to the accessibility tree"
    );
    assert!(
        DASHBOARD_HTML.contains(r#"aria-modal="true""#),
        "a modal dialog must declare aria-modal"
    );
    assert!(
        DASHBOARD_HTML.contains(r#"aria-labelledby="detail-title""#)
            && DASHBOARD_HTML.contains(r#"<h2 id="detail-title">"#),
        "the dialog must be named by its heading, and that heading must exist — \
         aria-labelledby pointing at nothing announces a bare \"dialog\""
    );
    assert!(
        !DASHBOARD_HTML.contains(r#"id="scrim""#),
        "a modal <dialog> paints its own ::backdrop; the #scrim div is dead weight \
         and, as a sibling of the dialog, is not inert while the dialog is open"
    );
    assert!(
        DASHBOARD_JS.contains("showModal()"),
        "the drawer must be opened with showModal(), not by removing `hidden`"
    );
    assert!(
        DASHBOARD_JS.contains("detail.close()"),
        "the drawer must be closed with close(), which is what fires the `close` \
         event the focus restoration hangs off"
    );
    assert!(
        !DASHBOARD_JS.contains(r#"$("scrim")"#),
        "nothing may still reference the removed #scrim element"
    );
    assert!(
        DASHBOARD_CSS.contains(".detail::backdrop"),
        "the backdrop that replaced #scrim has to be styled, or a modal dialog is \
         an un dimmed panel over an undimmed page"
    );
    assert!(
        !DASHBOARD_JS.contains(r#"ev.key === "Escape""#),
        "a modal <dialog> handles Escape itself and fires `cancel`; a document-level \
         Escape handler here would only ever be a second close() on a closed dialog"
    );
}

/// A click inside the panel must not read as a click on the backdrop.
///
/// The `::backdrop` is not an event target, so a click on it is retargeted to
/// the dialog element — the same target as its own padding and border. Keying
/// dismissal off `ev.target` alone would close the drawer on every click inside
/// it, so the coordinates are checked as well.
#[test]
fn backdrop_dismissal_distinguishes_a_click_inside_the_panel() {
    assert!(
        DASHBOARD_JS.contains(r#"if (ev.target !== $("detail")) return;"#),
        "dismissal must ignore clicks that landed on a field or the close button"
    );
    assert!(
        DASHBOARD_JS.contains("getBoundingClientRect()"),
        "dismissal must check where the click actually was, not only which element \
         received it"
    );
    assert!(
        DASHBOARD_JS.contains(r#"$("detail").addEventListener("click", dismissOnBackdrop);"#),
        "the dismissal handler must be bound to the dialog"
    );
}

/// The detail drawer is a keyboard trap: a screen-reader or keyboard user who
/// opens it must not be able to tab out into the page behind it, and closing it
/// must put them back on the row they came from (issue #714).
///
/// A modal dialog does not wrap Tab at the ends in any current browser, so this
/// has to be done by hand even once #715 lands.
///
/// Asserted against the shipped asset text rather than a DOM, because there is
/// no browser in CI — this pins the mechanisms the behaviour rests on, so a
/// refactor that drops any one of them fails here instead of shipping a drawer
/// that silently leaks focus.
#[test]
fn detail_drawer_traps_focus_and_restores_it() {
    // Focus in, on open.
    assert!(
        DASHBOARD_JS.contains(r#"$("detail").addEventListener("keydown", trapDetailFocus);"#),
        "the drawer's Tab handling must be bound to the drawer"
    );
    assert!(
        DASHBOARD_JS.contains("function focusDetail()"),
        "focus must move into the drawer when it opens"
    );
    assert!(
        DASHBOARD_JS.contains("focusDetail();"),
        "openDetail must call focusDetail, or focus stays on the row behind the drawer"
    );

    // Focus back out, on close — from the dialog's `close` event, so it covers
    // the close button, the backdrop, Escape and a re-open alike.
    assert!(
        DASHBOARD_JS.contains(r#"$("detail").addEventListener("close", onDetailClosed);"#),
        "focus restoration must hang off the dialog's `close` event, which every \
         route out of a <dialog> fires"
    );
    assert!(
        DASHBOARD_JS.contains("state.detailTrigger = trigger || null;"),
        "openDetail must remember the row that opened the drawer"
    );
    assert!(
        DASHBOARD_JS.contains("if (trigger && trigger.isConnected) {"),
        "focus must go back to that row, guarded on isConnected — focusing a \
         node detached by a re-render drops focus to <body>, which is the loss \
         this exists to prevent"
    );

    // The trap has to cover both directions and both escape routes.
    for needle in [
        r#"if (ev.key !== "Tab") return;"#,
        r#"if (ev.shiftKey && active === first) {"#,
        r#"} else if (!ev.shiftKey && active === last) {"#,
        r#"document.addEventListener("focusin", function (ev) {"#,
    ] {
        assert!(
            DASHBOARD_JS.contains(needle),
            "the focus trap is missing `{needle}`; Tab must wrap in both directions \
             and a focus that lands outside the drawer by any other route (a click \
             on the page behind it) must be pulled back"
        );
    }
}

/// Rows are what focus returns to, so they have to be reachable by keyboard in
/// the first place.
#[test]
fn payment_rows_are_keyboard_reachable_and_activate_on_enter() {
    assert!(
        DASHBOARD_JS.contains("tr.tabIndex = 0;"),
        "each payment row must be focusable, or there is nowhere to return focus to"
    );
    assert!(
        DASHBOARD_JS.contains("openDetail(p.id, tr);"),
        "rows must pass themselves to openDetail so focus can be handed back"
    );
}

const TEST_ADMIN_SECRET: &str = "test-admin-secret";

/// The stored theme must be applied before the first paint (issue #713).
///
/// A flash of the wrong palette on every load is the whole problem this
/// arrangement exists to avoid, and it comes back the moment the bootstrap is
/// deferred: `app.js` is a module, and a module runs after the document has
/// been parsed and painted. So the bootstrap is a separate classic script
/// referenced from `<head>`, and an inline block would not do either — the
/// dashboard CSP is `script-src 'self'` with no `unsafe-inline`, so it would be
/// blocked outright.
#[test]
fn theme_is_applied_before_first_paint() {
    assert!(
        DASHBOARD_HTML.contains(r#"<script src="/dashboard/theme.js"></script>"#),
        "the theme bootstrap must be a classic <script src> in the document, not a \
         module (deferred, so it runs after the first paint) and not an inline \
         block (blocked by the dashboard CSP)"
    );
    let head_end = DASHBOARD_HTML
        .find("</head>")
        .expect("the document has a <head>");
    let script_at = DASHBOARD_HTML
        .find(r#"<script src="/dashboard/theme.js"></script>"#)
        .expect("the theme bootstrap is referenced");
    assert!(
        script_at < head_end,
        "the theme bootstrap must be in <head>, or it cannot run before the body \
         is painted"
    );
    assert!(
        DASHBOARD_HTML.contains("type=\"module\" src=\"/dashboard/app.js\""),
        "app.js stays a module in the body; the two must not be merged, because a \
         module is deferred by definition"
    );

    // The bootstrap decides the theme on its own, with no help from app.js.
    assert!(
        DASHBOARD_THEME_JS.contains(r#"root.setAttribute("data-theme", theme);"#),
        "the bootstrap must set data-theme on <html> itself"
    );
    assert!(
        DASHBOARD_THEME_JS.contains(r#"window.localStorage.getItem(KEY)"#),
        "the bootstrap must read the stored preference before anything renders"
    );
    assert!(
        DASHBOARD_THEME_JS.contains(r#"value === "light" || value === "dark""#),
        "an unrecognised stored value must be treated as no preference, or a stale \
         or hand-edited value pins the page to a half-understood theme"
    );

    // The API key must not be reachable from the theme path. Both keys live in
    // the same origin's storage, so this is the assertion that keeps the theme
    // bootstrap from becoming a second reader of the credential.
    assert!(
        !DASHBOARD_THEME_JS.contains("apiKey"),
        "the theme bootstrap must not touch the API key"
    );
}

/// The override has to be a tri-state, not a binary.
///
/// No stored preference means the OS decides, and that has to stay true after
/// the control is used — pinning the theme that happened to be in effect at
/// first paint would turn "follow the OS" into a permanent override the
/// operator never asked for, and stop a later OS change from showing up.
#[test]
fn theme_override_is_tri_state_and_persisted() {
    assert!(
        DASHBOARD_JS.contains(r#"var THEME_KEY = "stellargate.theme";"#),
        "the preference must be stored under a stable key"
    );
    assert!(
        DASHBOARD_JS.contains("window.localStorage.setItem(THEME_KEY, theme);"),
        "choosing a theme must persist it"
    );
    assert!(
        DASHBOARD_JS.contains("window.localStorage.removeItem(THEME_KEY);"),
        "clearing the override must remove it, not store a resolved value"
    );
    assert!(
        DASHBOARD_JS.contains(r#"document.documentElement.removeAttribute("data-theme")"#),
        "clearing the override must drop the attribute so the CSS falls through \
         to prefers-color-scheme again"
    );
    assert!(
        DASHBOARD_CSS.contains(":root:not([data-theme])"),
        "the OS palette must stop applying once an override exists, or a stored \
         \"light\" choice loses to an OS preference of dark"
    );
    for selector in [r#"html[data-theme="light"]"#, r#"html[data-theme="dark"]"#] {
        assert!(
            DASHBOARD_CSS.contains(selector),
            "{selector} must exist, or the stored choice has nothing to select on"
        );
    }
    // `color-scheme` is what carries an explicit override to the parts of the
    // page CSS does not paint: the date pickers, the <select>, the scrollbar.
    assert!(
        DASHBOARD_CSS.contains("color-scheme: light dark;"),
        "the default must declare both palettes are supported so the OS picks"
    );
    for block in ["light", "dark"] {
        assert!(
            DASHBOARD_CSS.contains(&format!("color-scheme: {block};")),
            "an explicit {block} override must set color-scheme: {block}, or the \
             native controls keep the OS palette on a page that has changed"
        );
    }
}

/// The control is a toggle button, so its name is constant and `aria-pressed`
/// carries the state.
#[test]
fn theme_toggle_is_a_toggle_button_on_both_panels() {
    // Comments explain the control and name the attribute, so they are stripped
    // before counting attributes.
    let markup = strip_html_comments(DASHBOARD_HTML);
    let toggles = markup.matches("data-theme-toggle").count();
    assert_eq!(
        toggles, 2,
        "both the sign-in gate and the top bar need the control — whichever panel \
         is on screen should offer the override"
    );
    assert!(
        DASHBOARD_HTML.contains("class=\"visually-hidden\""),
        "the icon-only control needs a visually-hidden text label, or it has no \
         accessible name at all"
    );
    assert!(
        DASHBOARD_HTML.contains(r#"aria-pressed="false""#),
        "a toggle button must expose its state with aria-pressed"
    );
    assert!(
        DASHBOARD_JS.contains(r#"button.setAttribute("aria-pressed", dark ? "true" : "false")"#),
        "the control's state must follow the theme in effect"
    );
    assert!(
        DASHBOARD_JS.contains("function syncThemeUi()"),
        "both controls must be kept in step from one place"
    );
    assert!(
        DASHBOARD_JS.contains(r#"document.querySelectorAll("[data-theme-toggle]")"#),
        "controls must be bound by attribute, so a third one needs no new wiring"
    );
}

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

    for (path, content_type, body) in [
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
        // Applied before the first paint, so it must be a real, CSP-allowed
        // script of its own — see `theme_is_applied_before_first_paint` for why
        // it cannot be folded into app.js.
        (
            "/dashboard/theme.js",
            "text/javascript; charset=utf-8",
            DASHBOARD_THEME_JS,
        ),
    ] {
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

/// The format.js ES module must be served with the correct content type, the
/// dashboard CSP, and baseline security headers — the same contract as every
/// other dashboard asset (#725).
#[tokio::test]
async fn dashboard_format_js_served_with_correct_content_type_and_csp() {
    let server = test_server().await;

    let res = server.get("/dashboard/format.js").await;
    res.assert_status_ok();
    assert_eq!(
        res.header("content-type"),
        "text/javascript; charset=utf-8",
        "/dashboard/format.js served with the wrong content type"
    );
    assert_eq!(
        res.header("content-security-policy"),
        EXPECTED_CSP,
        "/dashboard/format.js must carry the dashboard CSP unchanged"
    );
    assert_eq!(
        res.header("x-content-type-options"),
        "nosniff",
        "/dashboard/format.js"
    );
    assert_eq!(
        res.header("referrer-policy"),
        "no-referrer",
        "/dashboard/format.js"
    );
    assert_eq!(
        res.header("cache-control"),
        "no-store",
        "/dashboard/format.js"
    );
    assert_eq!(
        res.text(),
        DASHBOARD_FORMAT_JS,
        "/dashboard/format.js must serve the include_str! asset byte-for-byte"
    );
}

/// Requesting an unknown path under /dashboard/ must return 404 with the
/// standard JSON error envelope, not an asset or an empty body (#725).
#[tokio::test]
async fn dashboard_unknown_asset_returns_404() {
    let server = test_server().await;

    for path in [
        "/dashboard/nonexistent.js",
        "/dashboard/unknown.css",
        "/dashboard/missing",
        "/dashboard/app.js.map",
        "/dashboard/../../etc/passwd",
    ] {
        let res = server.get(path).await;
        assert_eq!(
            res.status_code(),
            404,
            "{path} should return 404 for an unknown dashboard asset"
        );
        // The body must be the standard JSON error envelope, not an HTML page.
        let body: serde_json::Value = res.json();
        assert_eq!(
            body.get("code").and_then(|v| v.as_str()),
            Some("not_found"),
            "{path} error envelope must have code=not_found"
        );
    }
}

/// Each header must appear exactly once — a response builder that appends
/// instead of replacing would otherwise emit a duplicate `nosniff` alongside
/// the outer security-header layer's copy.
#[tokio::test]
async fn dashboard_security_headers_are_not_duplicated() {
    let server = test_server().await;

    for path in [
        "/dashboard",
        "/dashboard/app.css",
        "/dashboard/app.js",
        "/dashboard/format.js",
        "/dashboard/theme.js",
    ] {
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
