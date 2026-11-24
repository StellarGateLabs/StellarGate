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

const HTML: &str = include_str!("../static/dashboard.html");
const CSS: &str = include_str!("../static/dashboard.css");
const JS: &str = include_str!("../static/dashboard.js");
const I18N: &str = include_str!("../static/i18n.js");

const DASHBOARD_HTML: &str = include_str!("../static/dashboard.html");
const DASHBOARD_CSS: &str = include_str!("../static/dashboard.css");
const DASHBOARD_JS: &str = include_str!("../static/dashboard.js");
const DASHBOARD_THEME_JS: &str = include_str!("../static/dashboard-theme.js");

const DASHBOARD_STATE_JS: &str = include_str!("../static/state.js");

const TEST_ADMIN_SECRET: &str = "test-admin-secret";

fn make_config() -> Config {
    Config {
        port: 0,
        bind_addr: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
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
        expiry_sweep_interval_secs: 10,
        trustline_check_interval_secs: 3600,
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

/// All ES module siblings of `app.js`, as `(served_path, source_body)` pairs.
///
/// `every_dashboard_module_import_resolves_to_a_served_route` iterates this
/// list to verify every relative import in each module resolves to a route the
/// router actually serves. Add a new module here the moment it is created so
/// the check stays exhaustive.
const MODULES: &[(&str, &str)] = &[
    ("/dashboard/format.js", include_str!("../static/format.js")),
    (
        "/dashboard/session.js",
        include_str!("../static/session.js"),
    ),
    ("/dashboard/state.js", include_str!("../static/state.js")),
    ("/dashboard/keys.js", include_str!("../static/keys.js")),
];

/// Every `/dashboard/…` path the router actually serves.
///
/// Used by `every_dashboard_module_import_resolves_to_a_served_route` to
/// confirm that a relative import in a module has a matching route.
const JS_ROUTES: &[&str] = &[
    "/dashboard/app.js",
    "/dashboard/format.js",
    "/dashboard/session.js",
    "/dashboard/state.js",
    "/dashboard/keys.js",
    "/dashboard/vendor/qrcode.js",
];

#[test]
fn skip_link_is_first_focusable_in_app_and_targets_table() {
    let app = &HTML[HTML.find(r#"<div id="app""#).expect("app container")..];
    let skip = app
        .find(r##"<a class="skip-link" href="#payments""##)
        .expect("skip link");
    let first_focusable = ["<a ", "<button", "<input", "<select", "<textarea"]
        .iter()
        .filter_map(|tag| app.find(tag))
        .min()
        .expect("focusable element");
    assert_eq!(skip, first_focusable);
    assert!(HTML.contains(r#"<table id="payments""#));
}

#[test]
fn skip_link_is_visible_on_focus() {
    assert!(CSS.contains(".skip-link:focus"));
}

/// Keys defined in one locale block of `i18n.js`, e.g. `catalog_keys("en")`.
fn catalog_keys(locale: &str) -> Vec<String> {
    let start = I18N
        .find(&format!("\n    {locale}: {{"))
        .expect("locale block");
    let block = &I18N[start..];
    let block = &block[..block.find("\n    },").expect("block end")];
    block
        .lines()
        .filter_map(|line| line.trim().strip_prefix('"')?.split_once("\":"))
        .map(|(key, _)| key.to_string())
        .collect()
}

/// Every `prefix…"key"` occurrence in `source`.
fn used_keys(source: &str, prefix: &str) -> Vec<String> {
    source
        .match_indices(prefix)
        .filter(|(i, _)| {
            !source[..*i].ends_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.')
        })
        .map(|(i, _)| {
            let rest = &source[i + prefix.len()..];
            rest[..rest.find('"').unwrap()].to_string()
        })
        .collect()
}

#[test]
fn every_used_message_key_exists_in_catalog() {
    let en = catalog_keys("en");
    let mut used = used_keys(JS, "t(\"");
    for attr in [
        "data-i18n=\"",
        "data-i18n-placeholder=\"",
        "data-i18n-title=\"",
        "data-i18n-aria-label=\"",
    ] {
        used.extend(used_keys(HTML, attr));
    }
    assert!(
        used.len() > 20,
        "expected the UI strings to use the catalog"
    );
    for key in used {
        assert!(en.contains(&key), "missing message key: {key}");
    }
}

#[test]
fn extra_locale_covers_every_english_key() {
    assert_eq!(catalog_keys("en"), catalog_keys("es"));
}

#[test]
fn language_switcher_sets_lang_and_dir() {
    assert!(HTML.contains(r#"<select id="language""#));
    assert!(HTML.find("/dashboard/i18n.js") < HTML.find("/dashboard/app.js"));
    assert!(I18N.contains("navigator.language"));
    assert!(I18N.contains("root.lang =") && I18N.contains("root.dir ="));
}

#[test]
fn layout_uses_logical_properties() {
    for physical in [
        "left:",
        "right:",
        "-left",
        "-right:",
        "text-align: left",
        "text-align: right",
    ] {
        assert!(
            !CSS.contains(physical),
            "physical property in CSS: {physical}"
        );
    }
    assert!(CSS.contains("inset-inline-end: 0"));
}

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
            "return fetch(API_BASE + path, { method: opts.method || \"GET\", headers: headers, body: opts.body || undefined, signal: opts.signal }).then(",
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

#[test]
fn dashboard_print_styles_hide_chrome_and_keep_table_readable() {
    assert!(
        DASHBOARD_CSS.contains("@media print"),
        "print styles must be defined"
    );
    for needle in [
        ".topbar,",
        ".toolbar,",
        ".summary,",
        "#gate,",
        ".detail,",
        ".help,",
        ".payments th,",
        ".payments td {",
        "page-break-inside: avoid;",
    ] {
        assert!(
            DASHBOARD_CSS.contains(needle),
            "print stylesheet must keep the dashboard chrome hidden and the table readable: missing {needle}"
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

/// The admin key-management panel must be present in the markup (#767–#769).
///
/// The panel is the only place an operator can see, issue or revoke a
/// merchant's keys, so its controls and columns are pinned here: a rename in
/// the HTML that the script does not follow would leave a dead button rather
/// than a visible error.
#[test]
fn admin_key_management_markup_is_present() {
    let markup = strip_html_comments(DASHBOARD_HTML);

    // Merchant lookup (#767).
    assert!(
        markup.contains(r#"id="admin-merchant-id""#),
        "the admin panel needs a merchant ID input"
    );
    assert!(
        markup.contains(r#"id="admin-load-keys""#),
        "the admin panel needs a Load keys action"
    );
    assert!(
        markup.contains(r#"id="admin-issue-key""#),
        "the admin panel needs an Issue key action (#768)"
    );

    // Keys table columns (#767).
    for column in ["Key ID", "Prefix", "Created", "Revoked"] {
        assert!(
            markup.contains(column),
            "the keys table must render a {column:?} column"
        );
    }

    // One-time reveal panel (#768).
    assert!(
        markup.contains(r#"id="admin-key-reveal""#),
        "a newly issued key needs a reveal panel"
    );
    assert!(
        markup.contains(r#"id="admin-key-copy""#),
        "the reveal panel needs a copy button"
    );
    assert!(
        markup.to_lowercase().contains("shown only once"),
        "the reveal panel must warn that the key is shown only once"
    );

    // Revoke confirmation (#769).
    assert!(
        markup.contains(r#"id="admin-revoke-confirm""#),
        "revoking a key needs a confirmation control"
    );
    assert!(
        markup.to_lowercase().contains("type the key prefix"),
        "revocation must require the operator to type the key prefix"
    );
}

/// Sortable payment columns (#770): the headers are buttons and the active one
/// carries `aria-sort`.
#[test]
fn payment_columns_are_sortable_buttons() {
    let markup = strip_html_comments(DASHBOARD_HTML);

    assert!(
        markup.contains("data-sort-key"),
        "sortable column headers must be buttons carrying a sort key"
    );
    assert!(
        markup.contains(r#"aria-sort="none""#),
        "sortable headers must declare aria-sort, defaulting to none"
    );
    assert!(
        DASHBOARD_JS.contains(r#"setAttribute("aria-sort""#),
        "the active header's aria-sort must be updated when the sort changes"
    );
    assert!(
        DASHBOARD_JS.to_lowercase().contains("loaded rows"),
        "sorting is client-side over loaded rows only; the UI must say so when \
         more pages remain"
    );
}

/// No key material may be logged or placed in a URL by the admin panel
/// (#767–#769).
///
/// The existing `dashboard_never_puts_the_api_key_in_a_url_or_the_console`
/// check covers the operator's own credential; this one covers the merchant
/// keys the admin panel handles, which are equally secret.
#[test]
fn admin_key_material_is_never_logged_or_put_in_a_url() {
    for (name, source) in
        std::iter::once(("dashboard.js", DASHBOARD_JS)).chain(MODULES.iter().map(|(p, b)| (*p, *b)))
    {
        for forbidden in [
            "console.log",
            "console.debug",
            "console.info",
            "console.warn",
            "console.error",
        ] {
            assert!(
                !source.contains(forbidden),
                "{name} must not contain {forbidden:?}: key material must never \
                 reach a log line or the console"
            );
        }
        for forbidden in ["api_key=", "apikey=", "?key=", "?prefix="] {
            assert!(
                !source.contains(forbidden),
                "{name} must not contain {forbidden:?}: key material must never \
                 appear in a URL or query string"
            );
        }
    }
}

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

/// Colour contrast is checked numerically by `scripts/check-dashboard-contrast.mjs`
/// (issue #718) — this pins the structural preconditions that audit depends on,
/// so a failure means "the audit is looking in the wrong place" rather than
/// "the audit is broken".
#[test]
fn contrast_audit_can_find_every_theme_block() {
    // One palette per block, and the same tokens in each. The audit compares
    // block-by-block, so a token added to one and not another is exactly the
    // drift that would make a colour unreadable in the theme nobody was
    // looking at — and #719's `--skeleton-*` are the tokens most likely to
    // land that way.
    for selector in [
        ":root",
        ":root:not([data-theme])",
        r#"html[data-theme="light"]"#,
        r#"html[data-theme="dark"]"#,
    ] {
        assert!(
            DASHBOARD_CSS.contains(selector),
            "the contrast audit reads the `{selector}` block; it is missing"
        );
    }
    assert_eq!(
        DASHBOARD_CSS.matches("--control-border:").count(),
        4,
        "every theme block must define --control-border, or an interactive control \
         falls back to a colour no audit looked at"
    );
    assert_eq!(
        DASHBOARD_CSS.matches("--focus:").count(),
        4,
        "every theme block must define --focus; there was no focus colour at all \
         before #718, and the audit needs one per theme"
    );
}

/// The focus ring's `outline-offset` is load-bearing, not cosmetic.
///
/// `scripts/check-dashboard-contrast.mjs` deliberately does not pair the focus
/// colour against `--accent`, because a 2px offset puts a gap of the page
/// background between the ring and a filled control's own edge. Remove the
/// offset and that reasoning stops holding while the audit still passes, so the
/// invariant is asserted here.
#[test]
fn focus_ring_offset_is_preserved() {
    assert!(
        DASHBOARD_CSS.contains("outline-offset: 2px;"),
        "the focus ring needs a 2px offset: without it the ring touches an \
         accent-filled control and the contrast audit's reasoning about adjacent \
         colours no longer holds"
    );
    assert!(
        DASHBOARD_CSS.contains(":focus-visible {"),
        "focus must be indicated through :focus-visible, which every current \
         browser supports and which keeps a mouse click from leaving a ring"
    );
}

/// Decorative separators and interactive control boundaries are different jobs
/// and now have different tokens.
#[test]
fn control_boundaries_use_the_audited_token() {
    assert!(
        DASHBOARD_CSS.contains("border: 1px solid var(--control-border);"),
        "buttons and inputs must draw their edge from --control-border, the token \
         the contrast audit holds to 3:1"
    );
    for selector in ["input[type=\"date\"]", "select"] {
        assert!(
            DASHBOARD_CSS.contains(selector),
            "{selector} needs a rule: it previously had none at all, so its border \
             was drawn by the user agent in a colour this stylesheet never chose"
        );
    }
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
     img-src 'self'; \
     connect-src 'self'; \
     form-action 'none'; \
     frame-ancestors 'none'; \
     base-uri 'none'; \
     require-trusted-types-for 'script'";

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

/// Regression: #765. Printing must be available only for a loaded invoice.
#[test]
fn issue_765_prints_invoice_without_dashboard_chrome() {
    assert!(DASHBOARD_HTML.contains("id=\"invoice-print\""));
    assert!(DASHBOARD_HTML.contains("id=\"payment-invoice\""));
    assert!(DASHBOARD_JS.contains("window.print()"));
    assert!(DASHBOARD_JS.contains("selectedPaymentId !== id"));
    assert!(DASHBOARD_CSS.contains("#detail > :not(#payment-invoice)"));
    assert!(DASHBOARD_CSS.contains("@media print"));
}

/// Status must not be conveyed by colour alone (#821).
///
/// Each pill carries a distinct glyph alongside its text, so the status is
/// still readable in greyscale, under a colour-blindness simulator, and to a
/// screen reader that never sees the background colour at all.
#[test]
fn status_pills_carry_a_non_colour_cue() {
    // One glyph per status, defined once in the stylesheet.
    for status in ["succeeded", "pending", "failed", "expired"] {
        assert!(
            DASHBOARD_CSS.contains(&format!(".pill.{status}::before")),
            "the {status} pill needs a ::before glyph, or its status is carried \
             by colour alone (#821)"
        );
    }
    // The glyphs must actually differ, not be the same dot recoloured.
    let glyphs: Vec<&str> = ["succeeded", "pending", "failed", "expired"]
        .iter()
        .filter_map(|status| {
            let start = DASHBOARD_CSS.find(&format!(".pill.{status}::before"))?;
            let block = &DASHBOARD_CSS[start..];
            let block = &block[..block.find('}')?];
            block
                .split_once("content:")
                .map(|(_, rest)| rest.trim().trim_end_matches(';').trim())
        })
        .collect();
    assert_eq!(glyphs.len(), 4, "every status pill needs a content glyph");
    let mut unique = glyphs.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        glyphs.len(),
        "each status needs a distinct glyph, not one shape recoloured: {glyphs:?}"
    );
    // The glyph is decorative; the text already names the status.
    assert!(
        DASHBOARD_CSS.contains("aria-hidden") || DASHBOARD_HTML.contains("aria-hidden"),
        "a purely decorative glyph must be hidden from assistive technology"
    );
}

/// Windows High Contrast / `forced-colors` must keep pills, chips and focus
/// rings visible (#822).
///
/// In forced-colors mode the user agent overrides author colours, so anything
/// distinguished only by a background colour disappears. The rules below
/// re-draw those boundaries with system colour keywords, which the UA maps to
/// the user's chosen palette.
#[test]
fn forced_colors_mode_uses_system_colours() {
    assert!(
        DASHBOARD_CSS.contains("@media (forced-colors: active)"),
        "the dashboard needs a forced-colors block, or pills, chips and focus \
         rings vanish in Windows High Contrast (#822)"
    );
    let start = DASHBOARD_CSS
        .find("@media (forced-colors: active)")
        .expect("forced-colors block");
    let block = &DASHBOARD_CSS[start..];
    let block = &block[..block.find("\n}").map(|i| i + 2).unwrap_or(block.len())];
    for keyword in ["CanvasText", "Canvas", "Highlight", "ButtonText"] {
        assert!(
            block.contains(keyword),
            "the forced-colors block must use the system colour {keyword}, or it \
             is still relying on an author colour the UA has overridden"
        );
    }
    // Focus must stay visible: the ring is redrawn in a system colour.
    assert!(
        block.contains("outline") && block.contains("Highlight"),
        "focus indicators must be redrawn with a system colour in forced-colors \
         mode, or keyboard users lose the ring entirely (#822)"
    );
    // Pills and chips need a real border, since their fill is overridden.
    assert!(
        block.contains(".pill") && block.contains("border"),
        "pills must gain a border in forced-colors mode; their background fill \
         is overridden and nothing else marks the boundary"
    );
}

/// The layout must survive 200% text zoom and a 320px viewport (#823).
///
/// WCAG 1.4.4 and 1.4.10 require content to reflow without horizontal
/// scrolling. The toolbar wraps and the table scrolls inside its own container
/// rather than pushing the page wide.
#[test]
fn layout_reflows_at_320px_and_200_percent_zoom() {
    assert!(
        DASHBOARD_CSS.contains("flex-wrap: wrap"),
        "the toolbar must wrap, or its controls are clipped at 320px (#823)"
    );
    assert!(
        DASHBOARD_CSS.contains("overflow-x: auto"),
        "the payments table must scroll inside its own container rather than \
         widening the page at 320px (#823)"
    );
    assert!(
        DASHBOARD_CSS.contains("@media (max-width: 480px)")
            || DASHBOARD_CSS.contains("@media (max-width: 40rem)"),
        "a narrow-viewport block must exist to stack the toolbar and drawer \
         content at 320px (#823)"
    );
    // Nothing may pin a fixed width wider than the smallest supported viewport.
    assert!(
        !DASHBOARD_CSS.contains("min-width: 360px"),
        "a min-width above 320px forces horizontal scrolling at the smallest \
         supported viewport (#823)"
    );
}

/// Table rows must use `content-visibility: auto` to stay responsive with
/// thousands of loaded rows (#805).
///
/// After many "Load more" pages the DOM holds thousands of rows. Without
/// `content-visibility: auto` the browser lays out and paints every row
/// whether it is on screen or not, and scrolling janks. The property skips
/// that work for off-screen rows while keeping them in the DOM, so keyboard
/// navigation (j/k, Tab), find-in-page and screen-reader access all work
/// exactly as before.
///
/// `contain-intrinsic-size` is required alongside it: without a size estimate
/// an un-rendered row collapses to zero height and the scrollbar jumps as the
/// user scrolls into a row that suddenly renders at its real height.
#[test]
fn table_rows_use_content_visibility_for_responsiveness() {
    assert!(
        DASHBOARD_CSS.contains("content-visibility: auto;"),
        "payment rows must use content-visibility: auto so the browser skips \
         layout and paint for off-screen rows (#805)"
    );
    assert!(
        DASHBOARD_CSS.contains("contain-intrinsic-size: auto 43px;"),
        "each row needs a contain-intrinsic-size estimate so off-screen rows \
         do not collapse to zero height and cause scrollbar jumps (#805)"
    );
    // The mobile card layout stacks cells vertically, so the desktop intrinsic
    // height (43px) is far too short; the override must exist inside the
    // narrow-viewport media query.
    assert!(
        DASHBOARD_CSS.contains("contain-intrinsic-size: auto 240px;"),
        "the mobile card layout needs a larger contain-intrinsic-size because \
         each row is a stack of labelled cells rather than a single-line table \
         row (#805)"
    );
}

/// Regression: #964. Re-applies the table semantics that #928 (#818, #820)
/// shipped and a later merge reverted. Every header of the payments table
/// must be a column header, and the table needs a caption.
#[test]
fn issue_964_payments_table_has_caption_and_column_scopes() {
    let start = HTML
        .find("id=\"payments\"")
        .expect("payments table must exist in dashboard.html");
    let table = &HTML[start..];
    let table = &table[..table.find("</table>").expect("payments table must close")];

    assert!(
        table.contains("<caption id=\"payments-caption\""),
        "the payments table needs a caption so screen-reader users hear the \
         active filters (#964)"
    );
    // Count header cells exactly: `<th ` or `<th>`, so `<thead>` is not counted.
    let headers = table.matches("<th ").count() + table.matches("<th>").count();
    let scoped = table.matches("<th scope=\"col\"").count();
    assert!(headers > 0, "payments table must have headers");
    assert_eq!(
        headers, scoped,
        "every payments header must carry scope=\"col\" (#964)"
    );
}

/// Regression: #964. Payment rows must not be tab stops. Keyboard users open a
/// payment through the button in the Payment ID cell instead.
#[test]
fn issue_964_payment_rows_are_not_focusable() {
    let start = JS
        .find("function rowFor(")
        .expect("rowFor must exist in dashboard.js");
    let body = &JS[start..];
    let body = &body[..body
        .find("function webhookBadge(")
        .expect("rowFor must end")];

    assert!(
        !body.contains("tr.tabIndex") && !body.contains("tabindex"),
        "payment rows must not carry tabindex; the Payment ID button is the \
         keyboard control (#964)"
    );
    assert!(
        body.contains("el(\"button\", \"row-link mono\""),
        "the Payment ID cell must contain a real <button> (#964)"
    );
    assert!(
        !body.contains("tr.addEventListener(\"keydown\""),
        "the row keydown handler must go; the button's native activation \
         replaces it (#964)"
    );
}

/// Regression: #964. The caption must follow the active filters.
#[test]
fn issue_964_payments_caption_tracks_filters() {
    assert!(
        JS.contains("function updatePaymentsCaption()"),
        "dashboard.js must define updatePaymentsCaption (#964)"
    );
    assert!(
        JS.matches("updatePaymentsCaption();").count() >= 2,
        "the caption must refresh on filter changes and on row render (#964)"
    );
}

/// The favicon is a routed static file, so the CSP needs neither `data:` in
/// `img-src` (#798) nor an inline icon in the page, and Trusted Types are
/// enforced for script sinks (#799).
#[tokio::test]
async fn favicon_is_served_and_csp_is_tight() {
    let server = test_server().await;

    let res = server.get("/dashboard/favicon.svg").await;
    res.assert_status_ok();
    assert_eq!(res.header("content-type"), "image/svg+xml");
    let csp = res
        .header("content-security-policy")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(
        !csp.contains("data:"),
        "CSP must not allow data: URIs: {csp}"
    );
    assert!(csp.contains("require-trusted-types-for 'script'"), "{csp}");
    assert!(DASHBOARD_HTML.contains("/dashboard/favicon.svg"));
    assert!(!DASHBOARD_HTML.contains("data:image"));
}

/// Sign-out must empty the DOM containers that held payment data (#797).
#[test]
fn sign_out_wipes_payment_data() {
    let start = DASHBOARD_JS
        .find("function wipePaymentData")
        .expect("wipe fn");
    let body = &DASHBOARD_JS[start..start + 900];
    for id in ["rows", "summary", "detail-fields", "deliveries"] {
        assert!(body.contains(&format!("\"{id}\"")), "{id} not wiped");
    }
    assert!(DASHBOARD_JS.contains("loadedPayments: []"));
    assert!(DASHBOARD_JS.contains("wipePaymentData();"));
}

/// Page Visibility pauses polling and refreshes once on return (#803).
#[test]
fn polling_pauses_while_tab_hidden() {
    assert!(DASHBOARD_JS.contains("document.hidden"));
    assert!(DASHBOARD_JS.contains("visibilitychange"));
}

/// Assets carry an ETag, answer If-None-Match with 304, use `no-cache`, and
/// vary on Accept-Encoding (#801, #802).
#[tokio::test]
async fn dashboard_assets_support_etag_and_vary() {
    let server = test_server().await;
    let res = server.get("/dashboard/app.js").await;
    res.assert_status_ok();
    let etag = res.header("etag").to_str().unwrap().to_string();
    assert_eq!(res.header("cache-control"), "no-cache");
    assert_eq!(res.header("vary"), "Accept-Encoding");
    let res = server
        .get("/dashboard/app.js")
        .add_header("if-none-match", etag.as_str())
        .await;
    res.assert_status(axum::http::StatusCode::NOT_MODIFIED);
    assert_eq!(res.header("etag").to_str().unwrap(), etag);
}

/// gzip is negotiated for dashboard assets (#802).
#[tokio::test]
async fn dashboard_assets_are_gzip_compressed() {
    let server = test_server().await;
    let res = server
        .get("/dashboard/app.js")
        .add_header("accept-encoding", "gzip")
        .await;
    res.assert_status_ok();
    assert_eq!(res.header("content-encoding"), "gzip");
    assert_eq!(res.header("vary"), "Accept-Encoding");
}
