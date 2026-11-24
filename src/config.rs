use anyhow::Result;
use ipnet::IpNet;

/// How the service detects incoming on-chain payments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListenerMode {
    /// Subscribe to Horizon's Server-Sent-Events payment stream for near
    /// real-time settlement, with the interval poller running alongside as a
    /// reconciler for any events missed during reconnects.
    Stream,
    /// Only run the interval poller; no streaming connection is opened.
    Poll,
}

impl ListenerMode {
    /// Parse `STELLAR_LISTENER_MODE` from a raw env-var value.
    ///
    /// - Empty / unset → defaults to `Stream` (no error).
    /// - `"stream"` or `"poll"` (case-insensitive) → the chosen mode.
    /// - Any other non-empty value → `Err`, which aborts boot with a clear
    ///   message rather than silently falling back to a different mode.
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" => Ok(Self::Stream),
            "stream" => Ok(Self::Stream),
            "poll" => Ok(Self::Poll),
            other => Err(anyhow::anyhow!(
                "STELLAR_LISTENER_MODE={other:?} is not a recognised value. \
                 Valid values are \"stream\" or \"poll\". \
                 Fix the environment variable or remove it to use the default (\"stream\")."
            )),
        }
    }
}

/// A Stellar asset the gateway is configured to accept.
///
/// `issuer` is `None` for the native XLM asset; all other assets require an
/// issuer address. Configure via `ACCEPTED_ASSETS` as comma-separated entries
/// of the form `CODE` (native) or `CODE:ISSUER`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcceptedAsset {
    pub code: String,
    pub issuer: Option<String>,
}

impl AcceptedAsset {
    pub(crate) fn parse_list(raw: &str) -> Vec<Self> {
        raw.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|entry| {
                if let Some((code, issuer)) = entry.split_once(':') {
                    AcceptedAsset {
                        code: code.trim().to_uppercase(),
                        issuer: Some(issuer.trim().to_string()),
                    }
                } else {
                    AcceptedAsset {
                        code: entry.trim().to_uppercase(),
                        issuer: None,
                    }
                }
            })
            .collect()
    }

    /// The default allow-list used when `ACCEPTED_ASSETS` is unset, for the
    /// given `STELLAR_NETWORK` value.
    ///
    /// - `testnet` (and any non-`public` network): native XLM plus Circle's
    ///   testnet USDC issuer.
    /// - `public`: native XLM only. The testnet USDC issuer holds no real USDC
    ///   on mainnet, so accepting it by default would price intents in an
    ///   asset real customers can never pay with (issue #968). Mainnet assets
    ///   must be opted into explicitly via `ACCEPTED_ASSETS`.
    pub fn default_list_for_network(network: &str) -> Vec<Self> {
        let mut assets = vec![AcceptedAsset {
            code: "XLM".into(),
            issuer: None,
        }];
        if network != "public" {
            assets.push(AcceptedAsset {
                code: "USDC".into(),
                issuer: Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into()),
            });
        }
        assets
    }

    /// The testnet default allow-list. Equivalent to
    /// `default_list_for_network("testnet")`.
    pub fn default_list() -> Vec<Self> {
        Self::default_list_for_network("testnet")
    }
}

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    /// Address the HTTP listener binds to (`BIND_ADDR`, default `0.0.0.0`).
    /// `127.0.0.1` keeps the API reachable only locally, e.g. behind a reverse
    /// proxy on the same host; `::` listens on IPv6.
    pub bind_addr: std::net::IpAddr,
    pub database_url: String,
    pub network: String,
    pub horizon_url: String,
    pub gateway_public: String,
    /// Assets the gateway will accept, validated on POST /payments and in verify().
    /// Configure via ACCEPTED_ASSETS=XLM,USDC:GISSUER (comma-separated).
    pub accepted_assets: Vec<AcceptedAsset>,
    pub webhook_secret: String,
    pub webhook_retry_attempts: u32,
    /// Base delay between inline retry attempts, in milliseconds. This is the
    /// *first* step of an exponential schedule (`base * 2^(attempt-1)`, capped
    /// by [`Self::webhook_retry_max_delay_ms`]), not a fixed interval — a
    /// constant delay meant every delivery that failed at the same moment,
    /// which is what happens when a receiver goes down, retried in lockstep
    /// and hit it again at exactly the same instants as it tried to come back
    /// up (issue #318).
    pub webhook_retry_delay_ms: u64,
    /// Upper bound on one inline retry delay, in milliseconds. Without it the
    /// doubling above would push the last attempt of a long retry chain
    /// arbitrarily far out and keep a settlement waiting on it.
    pub webhook_retry_max_delay_ms: u64,
    pub allowed_webhook_schemes: Vec<String>,
    /// Per-attempt timeout for outbound webhook POSTs, in seconds. Each
    /// delivery attempt is bounded independently, so a slow receiver can't
    /// hold up the retry loop (or the reconciler) for more than this value.
    /// Defaults to 10 seconds — short enough to keep retries responsive while
    /// giving receivers a fair window to process the request.
    pub webhook_timeout_secs: u64,
    /// How often (seconds) the background redrive worker scans for stuck
    /// webhook deliveries (`pending`/`failed` rows left behind by a process
    /// that exited mid-delivery, or a receiver that was down when retries
    /// were exhausted). The worker's first pass runs immediately on startup,
    /// so a restart redrives without waiting a full interval.
    pub webhook_redrive_interval_secs: u64,
    /// Maximum number of redrive HTTP attempts in flight at once.
    pub webhook_redrive_concurrency: usize,
    /// Total attempts (inline + redrive) before a delivery is left `failed`
    /// permanently.
    pub webhook_redrive_max_attempts: u32,
    /// How long (seconds) a delivery must sit idle since its last attempt (or
    /// creation) before the redrive worker will touch it. Must comfortably
    /// exceed the worst-case inline delivery time
    /// (`webhook_retry_attempts * (webhook_timeout_secs + webhook_retry_delay_ms)`)
    /// so the worker never races a `dispatch()` call that is still in flight
    /// for the same row. Acts as a hard floor under the exponential backoff
    /// below — a row is never touched sooner than this, even on its very
    /// first redrive attempt.
    pub webhook_redrive_grace_secs: i64,
    /// Starting delay (seconds) of the exponential backoff applied to a
    /// delivery's *redrive* attempts after it has failed at least once
    /// (`initial * 2^(attempts-1)`, capped by `webhook_redrive_backoff_max_secs`).
    /// A row that has never been attempted (`attempts == 0`, left behind by a
    /// crash between insert and its first send) is exempt from this backoff
    /// and is only gated by `webhook_redrive_grace_secs`. Set to `0` to
    /// disable growth and redrive purely on the fixed grace window.
    pub webhook_redrive_backoff_initial_secs: i64,
    /// Upper bound (seconds) on the exponential backoff above, so a delivery
    /// that has failed many times still gets retried at a bounded cadence
    /// rather than being pushed further and further out.
    pub webhook_redrive_backoff_max_secs: i64,
    /// How often the retention worker prunes rows that have outlived their
    /// usefulness. Both tables below grow monotonically without it, so on a
    /// long-running deployment the disk is the only thing that stops them.
    pub retention_interval_secs: u64,
    /// Days to keep terminal (`delivered`/`failed`) webhook delivery rows.
    /// `0` disables pruning and keeps them forever.
    pub webhook_delivery_retention_days: i64,
    /// Days to keep idempotency keys. They only need to outlive the window in
    /// which a client might retry a create, so this can be short. `0` disables
    /// pruning.
    pub idempotency_retention_days: i64,
    pub poll_interval_secs: u64,
    /// How often (seconds) the expiry sweeper transitions overdue `pending`
    /// intents to `expired`. Independent of `POLL_INTERVAL_SECS`; defaults to
    /// it so existing deployments keep today's behaviour.
    pub expiry_sweep_interval_secs: u64,
    /// How often (seconds) the trustline checker re-verifies that every
    /// accepted asset still has a trustline on the gateway account. Independent
    /// of `RETENTION_INTERVAL_SECS` and `POLL_INTERVAL_SECS`.
    pub trustline_check_interval_secs: u64,
    /// Maximum number of Horizon pages a single poll cycle will walk before it
    /// yields and waits for the next tick. Bounds how long one cycle can
    /// monopolise the poller task while catching up on a large backlog; the
    /// cursor is checkpointed at every page boundary, so the next cycle picks
    /// up exactly where this one stopped. `0` means unlimited — one cycle runs
    /// until it is fully caught up (issue #226).
    pub poll_max_pages_per_cycle: u32,
    /// How long a payment intent stays `pending` before the expiry sweeper
    /// transitions it to `expired`. Counted from the intent's `created_at`.
    pub payment_ttl_secs: u64,
    /// Maximum number of requests per second allowed per client IP before the
    /// rate-limit middleware responds with `429 Too Many Requests`.
    pub rate_limit_requests_per_sec: u32,
    /// Maximum number of SQLite connections in the pool.
    /// WAL mode allows one writer + many readers, so keeping this modest avoids
    /// contention. Defaults to 10.
    pub db_pool_max_connections: u32,
    /// How long (ms) SQLite waits for a lock before returning SQLITE_BUSY.
    /// Must be > 0 to avoid immediate lock errors under concurrent writes.
    pub db_busy_timeout_ms: u64,
    /// Comma-separated list of allowed CORS origins, e.g. `https://app.example.com`.
    /// Required when `STELLAR_NETWORK=public`; optional (falls back to permissive) on testnet.
    pub cors_allowed_origins: Vec<String>,
    pub listener_mode: ListenerMode,
    /// Bypasses the SSRF guard's loopback/link-local/private/reserved IP check
    /// on `webhook_url` (the DNS resolution and http(s)-scheme check still
    /// run). Only for local development and tests that target a loopback mock
    /// server — never enable this in production.
    pub webhook_allow_private_targets: bool,
    /// Shared secret required (via the `X-Admin-Secret` header) to call
    /// `POST /merchants`. Empty disables provisioning entirely — the endpoint
    /// rejects every request rather than falling back to an open default.
    pub admin_provisioning_secret: String,
    /// Shared bearer token required (via `Authorization: Bearer <token>`) to
    /// call `GET /metrics`. Empty disables scraping entirely — the endpoint
    /// rejects every request rather than exposing webhook/auth counters to
    /// anonymous callers by default (issue #250).
    pub metrics_token: String,
    /// Per-request timeout for the whole API, in seconds. A request whose
    /// handler hasn't produced a response within this window is aborted with
    /// `408 Request Timeout`, so a slow client or a stuck handler can't tie up
    /// a connection indefinitely. Defaults to 30 seconds.
    pub request_timeout_secs: u64,
    /// How long (seconds) the Horizon SSE stream listener may go without
    /// receiving any bytes before it treats the connection as dead and
    /// reconnects. Horizon sends periodic keep-alive comment lines on its SSE
    /// endpoints, so an idle window is a reliable liveness signal — without
    /// it, a half-open connection (a NAT or load balancer dropping idle state
    /// without sending `RST`, or an upstream stall) leaves `stream.next()`
    /// waiting forever, silently degrading detection to the interval poller's
    /// cadence with no log line and no metric (issue #312). Defaults to 30
    /// seconds.
    pub stream_idle_timeout_secs: u64,
    /// CIDR blocks whose `X-Forwarded-For` / `X-Real-IP` headers are honoured
    /// for rate-limit bucketing and auth-log source attribution (issue #330).
    ///
    /// Forwarding headers are client-supplied, so they are trusted ONLY when
    /// the socket peer is one of these proxies; every other peer is attributed
    /// by its own address and its headers are ignored. Empty (the default)
    /// means no proxy is trusted and the headers are always ignored — the
    /// safe default for a directly-exposed gateway.
    pub trusted_proxy_cidrs: Vec<IpNet>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let database_url =
            std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite:stellargate.db".to_string());
        let network = std::env::var("STELLAR_NETWORK").unwrap_or_else(|_| "testnet".to_string());
        let horizon_url = std::env::var("STELLAR_HORIZON_URL")
            .unwrap_or_else(|_| default_horizon_url(&network).to_string());
        validate_network_horizon(&network, &horizon_url)?;
        let gateway_public =
            std::env::var("STELLAR_GATEWAY_PUBLIC").unwrap_or_else(|_| "UNCONFIGURED".to_string());
        let webhook_secret = Self::validate_webhook_secret(std::env::var("WEBHOOK_SECRET"))?;
        let allowed_webhook_schemes = parse_webhook_schemes(
            &std::env::var("ALLOWED_WEBHOOK_SCHEMES").unwrap_or_else(|_| "https".to_string()),
        )?;

        /* Independent of network: HTTPS is enforced unconditionally on
        `public` regardless of this list, but on any other network an
        operator who has widened this allow-list to include `http` is
        choosing to let webhook payloads transit in cleartext. That should
        never be silent (issue #306). */
        if allowed_webhook_schemes.iter().any(|s| s == "http") {
            tracing::warn!(
                "ALLOWED_WEBHOOK_SCHEMES includes \"http\": webhook deliveries to a \
                 plaintext endpoint are not encrypted in transit."
            );
        }

        // Every entry is validated and normalised now, so a bad entry aborts
        // boot with a clear message instead of silently never matching a
        // browser's `Origin` header (issue #970).
        let cors_allowed_origins: Vec<String> = std::env::var("CORS_ALLOWED_ORIGINS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(normalise_cors_origin)
            .collect::<Result<Vec<String>>>()?;

        if network == "public" && cors_allowed_origins.is_empty() {
            return Err(anyhow::anyhow!(
                "CORS_ALLOWED_ORIGINS must be set when STELLAR_NETWORK=public. \
                 Leaving it unset would allow any browser origin to access the public API."
            ));
        }

        let webhook_allow_private_targets: bool =
            parse_env("WEBHOOK_ALLOW_PRIVATE_TARGETS", false)?;

        // Refuse to boot on a public-network deployment with the SSRF guard
        // disabled. With WEBHOOK_ALLOW_PRIVATE_TARGETS=true any authenticated
        // merchant can craft a webhook_url that reaches cloud instance-metadata
        // endpoints (169.254.169.254), services bound to loopback, or hosts on
        // the internal network — and get a response oracle back through
        // GET /payments/:id/webhooks (issue #246).
        if network == "public" && webhook_allow_private_targets {
            return Err(anyhow::anyhow!(
                "WEBHOOK_ALLOW_PRIVATE_TARGETS must not be enabled when \
                 STELLAR_NETWORK=public. It disables the SSRF guard, letting any \
                 merchant's webhook_url reach cloud metadata endpoints \
                 (169.254.169.254) and internal services. This flag is only for \
                 local development and tests — remove it before deploying to \
                 production."
            ));
        }

        // Warn on any network so the flag cannot be left on unnoticed on a
        // staging or development host that shares configuration with production
        // (issue #246).
        if webhook_allow_private_targets {
            tracing::warn!(
                "WEBHOOK_ALLOW_PRIVATE_TARGETS=true: the SSRF guard is disabled. \
                 Webhook targets may reach loopback, link-local, and private-range \
                 addresses. This flag is only for local development and tests — \
                 never enable it in a deployment that handles real payments."
            );
        }

        let admin_provisioning_secret =
            Self::validate_admin_secret(std::env::var("ADMIN_PROVISIONING_SECRET"))?;
        let metrics_token = Self::validate_metrics_token(std::env::var("METRICS_TOKEN"))?;

        let poll_interval_secs: u64 = parse_env("POLL_INTERVAL_SECS", 10)?;
        // Computed before `network` is moved into the struct below.
        let accepted_assets = {
            let raw = std::env::var("ACCEPTED_ASSETS").unwrap_or_default();
            if raw.is_empty() {
                AcceptedAsset::default_list_for_network(&network)
            } else {
                AcceptedAsset::parse_list(&raw)
            }
        };
        tracing::info!(
            network = %network,
            accepted_assets = %accepted_assets
                .iter()
                .map(|a| match &a.issuer {
                    Some(issuer) => format!("{}:{issuer}", a.code),
                    None => a.code.clone(),
                })
                .collect::<Vec<_>>()
                .join(","),
            "effective ACCEPTED_ASSETS allow-list"
        );

        let config = Self {
            port: parse_env("PORT", 3000)?,
            bind_addr: parse_env(
                "BIND_ADDR",
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            )?,
            database_url,
            network,
            horizon_url,
            gateway_public,
            accepted_assets,
            webhook_secret,
            allowed_webhook_schemes,
            webhook_retry_attempts: parse_env("WEBHOOK_RETRY_ATTEMPTS", 3)?,
            webhook_retry_delay_ms: parse_env("WEBHOOK_RETRY_DELAY_MS", 5000)?,
            webhook_retry_max_delay_ms: parse_env("WEBHOOK_RETRY_MAX_DELAY_MS", 60_000)?,
            webhook_timeout_secs: parse_env("WEBHOOK_TIMEOUT_SECS", 10)?,
            webhook_redrive_interval_secs: parse_env("WEBHOOK_REDRIVE_INTERVAL_SECS", 30)?,
            webhook_redrive_concurrency: parse_env("WEBHOOK_REDRIVE_CONCURRENCY", 4)?,
            webhook_redrive_max_attempts: parse_env("WEBHOOK_REDRIVE_MAX_ATTEMPTS", 8)?,
            webhook_redrive_grace_secs: parse_env("WEBHOOK_REDRIVE_GRACE_SECS", 60)?,
            webhook_redrive_backoff_initial_secs: parse_env(
                "WEBHOOK_REDRIVE_BACKOFF_INITIAL_SECS",
                30,
            )?,
            webhook_redrive_backoff_max_secs: parse_env("WEBHOOK_REDRIVE_BACKOFF_MAX_SECS", 900)?,
            retention_interval_secs: parse_env("RETENTION_INTERVAL_SECS", 3600)?,
            webhook_delivery_retention_days: parse_env("WEBHOOK_DELIVERY_RETENTION_DAYS", 30)?,
            idempotency_retention_days: parse_env("IDEMPOTENCY_RETENTION_DAYS", 7)?,
            poll_interval_secs,
            expiry_sweep_interval_secs: parse_env(
                "EXPIRY_SWEEP_INTERVAL_SECS",
                poll_interval_secs,
            )?,
            trustline_check_interval_secs: parse_env("TRUSTLINE_CHECK_INTERVAL_SECS", 3600)?,
            poll_max_pages_per_cycle: parse_env("POLL_MAX_PAGES_PER_CYCLE", 50)?,
            payment_ttl_secs: parse_env("PAYMENT_TTL_SECS", 3600)?,
            rate_limit_requests_per_sec: parse_env("RATE_LIMIT_REQUESTS_PER_SEC", 10)?,
            db_pool_max_connections: parse_env("DB_POOL_MAX_CONNECTIONS", 10)?,
            db_busy_timeout_ms: parse_env("DB_BUSY_TIMEOUT_MS", 5000)?,
            cors_allowed_origins,
            listener_mode: ListenerMode::parse(
                &std::env::var("STELLAR_LISTENER_MODE").unwrap_or_default(),
            )?,
            webhook_allow_private_targets,
            admin_provisioning_secret,
            metrics_token,
            request_timeout_secs: parse_env("REQUEST_TIMEOUT_SECS", 30)?,
            stream_idle_timeout_secs: parse_env("STREAM_IDLE_TIMEOUT_SECS", 30)?,
            trusted_proxy_cidrs: parse_cidrs(
                &std::env::var("TRUSTED_PROXY_CIDRS").unwrap_or_default(),
            )?,
        };
        config.validate_addresses()?;
        config.validate_timing()?;
        Ok(config)
    }

    /// True once a real gateway wallet has been configured. Until then the
    /// Horizon poller stays idle rather than scanning the placeholder account.
    pub fn gateway_configured(&self) -> bool {
        !self.gateway_public.is_empty() && self.gateway_public != "UNCONFIGURED"
    }

    /// Reject configured Stellar addresses — the gateway account and any asset
    /// issuers — that are not valid strkeys, so a typo fails fast at boot rather
    /// than silently producing unpayable intents. The unconfigured placeholder
    /// is left alone; the poller stays idle until a real key is provided.
    fn validate_addresses(&self) -> Result<()> {
        if self.gateway_configured() {
            crate::strkey::validate_account_id(&self.gateway_public).map_err(|e| {
                anyhow::anyhow!(
                    "STELLAR_GATEWAY_PUBLIC ({}) is not a valid Stellar account address: {e}",
                    self.gateway_public
                )
            })?;
        }
        for asset in &self.accepted_assets {
            if let Some(issuer) = &asset.issuer {
                crate::strkey::validate_account_id(issuer).map_err(|e| {
                    anyhow::anyhow!(
                        "issuer for asset {} ({}) is not a valid Stellar account address: {e}",
                        asset.code,
                        issuer
                    )
                })?;
            }
        }
        Ok(())
    }

    /// Cross-validate timing fields to catch nonsensical combinations that
    /// would cause silent misbehaviour at runtime:
    ///
    /// - `POLL_INTERVAL_SECS == 0` → infinite tight loop, 100 % CPU
    /// - `PAYMENT_TTL_SECS == 0` → every intent expires the moment it is created
    /// - `PAYMENT_TTL_SECS < POLL_INTERVAL_SECS` → intents expire before the
    ///   poller ever scans them, so payments land but are never matched
    /// - `WEBHOOK_RETRY_ATTEMPTS == 0` → webhooks are never delivered
    /// - `WEBHOOK_RETRY_DELAY_MS == 0` with retries > 1 → retries hammer the
    ///   target endpoint with no back-off
    /// - `REQUEST_TIMEOUT_SECS == 0` → every request is aborted immediately
    /// - `WEBHOOK_TIMEOUT_SECS == 0` → every webhook attempt fails immediately
    /// - `WEBHOOK_REDRIVE_MAX_ATTEMPTS <= WEBHOOK_RETRY_ATTEMPTS` → the redrive
    ///   worker never selects an inline-failed delivery
    /// - `DB_BUSY_TIMEOUT_MS == 0` → any write that meets another writer fails
    ///   immediately with `SQLITE_BUSY`
    /// - any negative `WEBHOOK_DELIVERY_RETENTION_DAYS`, `IDEMPOTENCY_RETENTION_DAYS`,
    ///   `WEBHOOK_REDRIVE_GRACE_SECS`, `WEBHOOK_REDRIVE_BACKOFF_INITIAL_SECS` or
    ///   `WEBHOOK_REDRIVE_BACKOFF_MAX_SECS` → a negative window or delay is never meaningful
    /// - `WEBHOOK_REDRIVE_BACKOFF_MAX_SECS < WEBHOOK_REDRIVE_BACKOFF_INITIAL_SECS`
    ///   → the cap would silently override the starting delay, so backoff
    ///   never actually grows
    fn validate_timing(&self) -> Result<()> {
        if self.expiry_sweep_interval_secs == 0 {
            return Err(anyhow::anyhow!(
                "EXPIRY_SWEEP_INTERVAL_SECS must be > 0 (got 0). \
                 A zero interval creates a tight sweep loop at 100% CPU."
            ));
        }

        if self.trustline_check_interval_secs == 0 {
            return Err(anyhow::anyhow!(
                "TRUSTLINE_CHECK_INTERVAL_SECS must be > 0 (got 0). \
                 A zero interval creates a tight Horizon-polling loop."
            ));
        }

        /* These are parsed as i64 but are durations or day counts. A negative
        value is never meaningful: a negative retention builds the SQLite
        modifier `--5 days`, which makes pruning silently match nothing, and a
        negative grace or backoff makes redrive eligible immediately, racing
        in-flight inline dispatches (issue #975). Zero keeps its documented
        meaning (retention disabled, backoff growth disabled). */
        for (name, value) in [
            (
                "WEBHOOK_DELIVERY_RETENTION_DAYS",
                self.webhook_delivery_retention_days,
            ),
            (
                "IDEMPOTENCY_RETENTION_DAYS",
                self.idempotency_retention_days,
            ),
            (
                "WEBHOOK_REDRIVE_GRACE_SECS",
                self.webhook_redrive_grace_secs,
            ),
            (
                "WEBHOOK_REDRIVE_BACKOFF_INITIAL_SECS",
                self.webhook_redrive_backoff_initial_secs,
            ),
            (
                "WEBHOOK_REDRIVE_BACKOFF_MAX_SECS",
                self.webhook_redrive_backoff_max_secs,
            ),
        ] {
            if value < 0 {
                return Err(anyhow::anyhow!(
                    "{name} must be >= 0 (got {value}). Negative values are never meaningful \
                     for this setting; use 0 where 0 is documented to disable it."
                ));
            }
        }

        if self.poll_interval_secs == 0 {
            return Err(anyhow::anyhow!(
                "POLL_INTERVAL_SECS must be > 0 (got 0). \
                 A zero interval creates a tight polling loop at 100% CPU."
            ));
        }

        if self.payment_ttl_secs == 0 {
            return Err(anyhow::anyhow!(
                "PAYMENT_TTL_SECS must be > 0 (got 0). \
                 A zero TTL expires every payment intent immediately on creation."
            ));
        }

        if self.payment_ttl_secs < self.poll_interval_secs {
            return Err(anyhow::anyhow!(
                "PAYMENT_TTL_SECS ({}) must be >= POLL_INTERVAL_SECS ({}). \
                 With the current settings, a payment intent would expire before \
                 the poller ever gets a chance to detect it.",
                self.payment_ttl_secs,
                self.poll_interval_secs
            ));
        }

        if self.webhook_retry_attempts == 0 {
            return Err(anyhow::anyhow!(
                "WEBHOOK_RETRY_ATTEMPTS must be > 0 (got 0). \
                 Zero attempts means webhooks are silently never delivered."
            ));
        }

        if self.webhook_retry_attempts > 1 && self.webhook_retry_delay_ms == 0 {
            return Err(anyhow::anyhow!(
                "WEBHOOK_RETRY_DELAY_MS must be > 0 when WEBHOOK_RETRY_ATTEMPTS ({}) > 1. \
                 A zero delay causes retry bursts that hammer the target endpoint.",
                self.webhook_retry_attempts
            ));
        }

        if self.webhook_retry_max_delay_ms < self.webhook_retry_delay_ms {
            return Err(anyhow::anyhow!(
                "WEBHOOK_RETRY_MAX_DELAY_MS ({}) must be >= WEBHOOK_RETRY_DELAY_MS ({}). \
                 With the current settings the cap would override the starting delay and the \
                 inline retry backoff would never actually grow.",
                self.webhook_retry_max_delay_ms,
                self.webhook_retry_delay_ms
            ));
        }

        /* WEBHOOK_REDRIVE_MAX_ATTEMPTS is a total that includes the inline
        attempts already recorded by `dispatch()`. The redrive worker only
        selects rows with `attempts < WEBHOOK_REDRIVE_MAX_ATTEMPTS`, so a cap at
        or below the inline budget means no inline-failed delivery is ever
        redriven, and the worker silently does nothing (issue #974). */
        if self.webhook_redrive_max_attempts <= self.webhook_retry_attempts {
            return Err(anyhow::anyhow!(
                "WEBHOOK_REDRIVE_MAX_ATTEMPTS ({}) must be > WEBHOOK_RETRY_ATTEMPTS ({}). \
                 WEBHOOK_REDRIVE_MAX_ATTEMPTS is a total that includes the inline attempts, \
                 so with this setting no delivery that failed inline would ever be redriven.",
                self.webhook_redrive_max_attempts,
                self.webhook_retry_attempts
            ));
        }

        /* The redrive grace window has to clear the worst case a `dispatch()`
        call can take, or the worker starts a second delivery for a row whose
        first one is still in flight. Making the inline delay exponential
        changed that arithmetic — the old comparison assumed a constant delay
        (issue #238, coordinating with #318). */
        let worst_case_inline = self.worst_case_inline_delivery_secs();
        if self.webhook_redrive_grace_secs < worst_case_inline as i64 {
            return Err(anyhow::anyhow!(
                "WEBHOOK_REDRIVE_GRACE_SECS ({}) is below the worst-case inline delivery time \
                 ({worst_case_inline}s). With the current settings the redrive worker could pick \
                 up a delivery whose inline dispatch is still running and send it twice. \
                 The inline budget is WEBHOOK_RETRY_ATTEMPTS ({}) attempts of up to \
                 WEBHOOK_TIMEOUT_SECS ({}s) each, plus the exponential retry delays \
                 (WEBHOOK_RETRY_DELAY_MS {}ms doubling to at most \
                 WEBHOOK_RETRY_MAX_DELAY_MS {}ms).",
                self.webhook_redrive_grace_secs,
                self.webhook_retry_attempts,
                self.webhook_timeout_secs,
                self.webhook_retry_delay_ms,
                self.webhook_retry_max_delay_ms
            ));
        }

        if self.request_timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "REQUEST_TIMEOUT_SECS must be > 0 (got 0). \
                 A zero timeout would abort every request immediately."
            ));
        }

        if self.stream_idle_timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "STREAM_IDLE_TIMEOUT_SECS must be > 0 (got 0). \
                 A zero timeout would make the stream listener reconnect \
                 continuously instead of tolerating any gap between events."
            ));
        }

        if self.webhook_timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "WEBHOOK_TIMEOUT_SECS must be > 0 (got 0). \
                 A zero timeout makes every webhook attempt fail immediately, so every \
                 delivery exhausts its retry budget and ends failed."
            ));
        }

        if self.db_busy_timeout_ms == 0 {
            return Err(anyhow::anyhow!(
                "DB_BUSY_TIMEOUT_MS must be > 0 (got 0). \
                 A zero busy timeout makes any write that meets another writer fail \
                 immediately with SQLITE_BUSY, which surfaces as 500s and failed settlements \
                 under ordinary concurrency."
            ));
        }

        if self.webhook_redrive_backoff_max_secs < self.webhook_redrive_backoff_initial_secs {
            return Err(anyhow::anyhow!(
                "WEBHOOK_REDRIVE_BACKOFF_MAX_SECS ({}) must be >= WEBHOOK_REDRIVE_BACKOFF_INITIAL_SECS ({}). \
                 With the current settings the cap would override the starting delay and backoff \
                 would never actually grow.",
                self.webhook_redrive_backoff_max_secs,
                self.webhook_redrive_backoff_initial_secs
            ));
        }

        Ok(())
    }

    /// Longest a single `webhook::dispatch` call can take, in seconds, rounded
    /// up.
    ///
    /// Every attempt may burn a full `webhook_timeout_secs`, and each gap
    /// between attempts is bounded by the exponential schedule
    /// `retry_delay(n) <= min(base * 2^(n-1), max)`. Jitter only ever shortens
    /// a gap, so the un-jittered ceiling is the worst case.
    ///
    /// This is what `WEBHOOK_REDRIVE_GRACE_SECS` has to clear for the redrive
    /// worker never to race a live dispatch for the same row (issues #238,
    /// #318). Before the delay became exponential the bound was simply
    /// `attempts * (timeout + delay)`.
    pub fn worst_case_inline_delivery_secs(&self) -> u64 {
        let attempts = self.webhook_retry_attempts.max(1) as u64;
        let timeouts = attempts.saturating_mul(self.webhook_timeout_secs);

        let mut delays_ms: u64 = 0;
        for attempt in 1..attempts {
            let factor = 2u64.saturating_pow(attempt as u32 - 1);
            let step = self
                .webhook_retry_delay_ms
                .saturating_mul(factor)
                .min(self.webhook_retry_max_delay_ms);
            delays_ms = delays_ms.saturating_add(step);
        }

        // Round the delay total up to whole seconds; a sub-second remainder
        // still has to fit inside the grace window.
        timeouts.saturating_add(delays_ms.div_ceil(1_000))
    }

    fn validate_webhook_secret(raw_secret: Result<String, std::env::VarError>) -> Result<String> {
        let secret = match raw_secret {
            Ok(s) => s,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "WEBHOOK_SECRET environment variable is missing"
                ));
            }
        };

        if secret.is_empty() {
            return Err(anyhow::anyhow!("WEBHOOK_SECRET cannot be empty"));
        }
        if secret.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "WEBHOOK_SECRET cannot contain only whitespace"
            ));
        }
        // Reject known placeholder values that might be copied verbatim from
        // .env.example or documentation.
        const WEBHOOK_PLACEHOLDERS: &[&str] = &[
            "default-secret",
            "your_webhook_signing_secret",
            "REPLACE_ME_webhook_signing_secret",
        ];
        if WEBHOOK_PLACEHOLDERS.contains(&secret.as_str()) || secret.starts_with("REPLACE_ME_") {
            return Err(anyhow::anyhow!(
                "WEBHOOK_SECRET is set to a known placeholder value ({secret:?}). \
                 Replace it with a strong, randomly-generated secret."
            ));
        }
        if secret.len() < 32 {
            return Err(anyhow::anyhow!(
                "WEBHOOK_SECRET must be at least 32 characters long (got {})",
                secret.len()
            ));
        }

        Ok(secret)
    }

    /// Validate `ADMIN_PROVISIONING_SECRET` (issue #245).
    ///
    /// * Empty / absent → provisioning disabled; log at `info` and return `""`.
    /// * Non-empty but short (< 32 chars) → abort boot.
    /// * Non-empty but a known placeholder → abort boot.
    /// * Otherwise → return the secret unchanged.
    ///
    /// The asymmetry with `validate_webhook_secret` (which requires the
    /// variable to be present) is intentional: an empty `ADMIN_PROVISIONING_SECRET`
    /// is a valid configuration choice that disables the `POST /merchants`
    /// endpoint entirely. A non-empty but weak value is always a mistake,
    /// because `require_admin_secret` guards merchant provisioning and the
    /// whole API-key lifecycle.
    fn validate_admin_secret(raw_secret: Result<String, std::env::VarError>) -> Result<String> {
        // Absent is treated the same as empty: provisioning disabled.
        let secret = raw_secret.unwrap_or_default();

        if secret.is_empty() {
            tracing::info!(
                "ADMIN_PROVISIONING_SECRET is not set — \
                 merchant provisioning is disabled (POST /merchants returns 401). \
                 Set the variable to a strong random value to enable it."
            );
            return Ok(String::new());
        }

        // Reject placeholder values that an operator might copy from documentation
        // or a template without changing.
        const ADMIN_PLACEHOLDERS: &[&str] = &[
            "admin",
            "secret",
            "changeme",
            "password",
            "test",
            "admin123",
            "your_admin_provisioning_secret",
            "REPLACE_ME_admin_provisioning_secret",
        ];
        if ADMIN_PLACEHOLDERS.contains(&secret.as_str())
            || secret.starts_with("REPLACE_ME_")
            || secret.eq_ignore_ascii_case("admin")
            || secret.eq_ignore_ascii_case("secret")
            || secret.eq_ignore_ascii_case("changeme")
        {
            return Err(anyhow::anyhow!(
                "ADMIN_PROVISIONING_SECRET is set to a known placeholder value. \
                 Replace it with a strong, randomly-generated secret \
                 (e.g. `openssl rand -hex 32`)."
            ));
        }

        if secret.len() < 32 {
            return Err(anyhow::anyhow!(
                "ADMIN_PROVISIONING_SECRET must be at least 32 characters long \
                 (got {}). Use a randomly-generated value \
                 (e.g. `openssl rand -hex 32`).",
                secret.len()
            ));
        }

        Ok(secret)
    }

    /// Validate `METRICS_TOKEN`. Mirrors [`Self::validate_admin_secret`]: an
    /// unset/empty value disables `GET /metrics` entirely (every scrape gets
    /// `401`) rather than falling back to the previously-open default
    /// (issue #250) — an operator must opt in to exposing the endpoint.
    fn validate_metrics_token(raw_token: Result<String, std::env::VarError>) -> Result<String> {
        // Absent is treated the same as empty: scraping disabled.
        let token = raw_token.unwrap_or_default();

        if token.is_empty() {
            tracing::info!(
                "METRICS_TOKEN is not set — GET /metrics is disabled (returns 401). \
                 Set the variable to a strong random value to enable scraping."
            );
            return Ok(String::new());
        }

        // Reject placeholder values that an operator might copy from documentation
        // or a template without changing.
        const METRICS_PLACEHOLDERS: &[&str] = &[
            "metrics",
            "secret",
            "changeme",
            "password",
            "test",
            "your_metrics_token",
            "REPLACE_ME_metrics_token",
        ];
        if METRICS_PLACEHOLDERS.contains(&token.as_str())
            || token.starts_with("REPLACE_ME_")
            || token.eq_ignore_ascii_case("metrics")
            || token.eq_ignore_ascii_case("secret")
            || token.eq_ignore_ascii_case("changeme")
        {
            return Err(anyhow::anyhow!(
                "METRICS_TOKEN is set to a known placeholder value. \
                 Replace it with a strong, randomly-generated secret \
                 (e.g. `openssl rand -hex 32`)."
            ));
        }

        if token.len() < 32 {
            return Err(anyhow::anyhow!(
                "METRICS_TOKEN must be at least 32 characters long \
                 (got {}). Use a randomly-generated value \
                 (e.g. `openssl rand -hex 32`).",
                token.len()
            ));
        }

        Ok(token)
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("port", &self.port)
            .field("database_url", &self.database_url)
            .field("network", &self.network)
            .field("horizon_url", &self.horizon_url)
            .field("gateway_public", &self.gateway_public)
            .field("accepted_assets", &self.accepted_assets)
            .field("webhook_secret", &"***")
            .field("webhook_retry_attempts", &self.webhook_retry_attempts)
            .field("webhook_retry_delay_ms", &self.webhook_retry_delay_ms)
            .field(
                "webhook_retry_max_delay_ms",
                &self.webhook_retry_max_delay_ms,
            )
            .field("webhook_timeout_secs", &self.webhook_timeout_secs)
            .field(
                "webhook_redrive_interval_secs",
                &self.webhook_redrive_interval_secs,
            )
            .field(
                "webhook_redrive_concurrency",
                &self.webhook_redrive_concurrency,
            )
            .field(
                "webhook_redrive_max_attempts",
                &self.webhook_redrive_max_attempts,
            )
            .field(
                "webhook_redrive_grace_secs",
                &self.webhook_redrive_grace_secs,
            )
            .field(
                "webhook_redrive_backoff_initial_secs",
                &self.webhook_redrive_backoff_initial_secs,
            )
            .field(
                "webhook_redrive_backoff_max_secs",
                &self.webhook_redrive_backoff_max_secs,
            )
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field(
                "expiry_sweep_interval_secs",
                &self.expiry_sweep_interval_secs,
            )
            .field(
                "trustline_check_interval_secs",
                &self.trustline_check_interval_secs,
            )
            .field("poll_max_pages_per_cycle", &self.poll_max_pages_per_cycle)
            .field("payment_ttl_secs", &self.payment_ttl_secs)
            .field(
                "rate_limit_requests_per_sec",
                &self.rate_limit_requests_per_sec,
            )
            .field("db_pool_max_connections", &self.db_pool_max_connections)
            .field("db_busy_timeout_ms", &self.db_busy_timeout_ms)
            .field("cors_allowed_origins", &self.cors_allowed_origins)
            .field("listener_mode", &self.listener_mode)
            .field(
                "webhook_allow_private_targets",
                &self.webhook_allow_private_targets,
            )
            .field("admin_provisioning_secret", &"***")
            .field("metrics_token", &"***")
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("stream_idle_timeout_secs", &self.stream_idle_timeout_secs)
            .field("trusted_proxy_cidrs", &self.trusted_proxy_cidrs)
            .finish()
    }
}

/// Validate one `CORS_ALLOWED_ORIGINS` entry and return its canonical form
/// (issue #970).
///
/// Browsers send `Origin` as `scheme://host[:port]`, with no path, query,
/// fragment, credentials or trailing slash. An entry in any other shape passed
/// the old header-value check but never matched a request, so the dashboard or
/// merchant frontend failed with an opaque CORS error. Those entries are
/// rejected here instead.
///
/// One trailing `/` is tolerated and stripped, since it is the most common
/// copy-paste form. The result is the serialised origin, so
/// `HTTPS://App.Example.com:443/` becomes `https://app.example.com`, the same
/// string a browser sends.
fn normalise_cors_origin(entry: &str) -> Result<String> {
    let invalid = |reason: &str| {
        anyhow::anyhow!(
            "CORS_ALLOWED_ORIGINS contains an invalid origin {entry:?}: {reason}. \
             Use the form https://app.example.com (scheme, host and optional port, \
             no path). Fix or remove the bad entry."
        )
    };

    if entry == "*" {
        return Err(invalid(
            "wildcards are not supported; list each origin explicitly",
        ));
    }

    let candidate = entry.strip_suffix('/').unwrap_or(entry);
    if candidate.ends_with('/') {
        return Err(invalid("more than one trailing slash"));
    }

    let url = reqwest::Url::parse(candidate)
        .map_err(|_| invalid("expected a full URL with a scheme, e.g. https://app.example.com"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("the scheme must be http or https"));
    }
    if url.host_str().is_none() {
        return Err(invalid("the host is missing"));
    }
    if url.path() != "/" {
        return Err(invalid("a path is not allowed"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("query strings and fragments are not allowed"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("credentials are not allowed"));
    }

    Ok(url.origin().ascii_serialization())
}

/// Passphrase of the Stellar network `STELLAR_NETWORK` names. Like the default
/// Horizon URL and accepted-asset list, anything other than `public` is treated
/// as testnet.
pub fn network_passphrase(network: &str) -> &'static str {
    if network == "public" {
        "Public Global Stellar Network ; September 2015"
    } else {
        "Test SDF Network ; September 2015"
    }
}

/// The Horizon endpoint used when `STELLAR_HORIZON_URL` is unset, chosen to
/// match `STELLAR_NETWORK`. Without this, a `public` gateway with no explicit
/// Horizon URL silently watched the testnet ledger for mainnet accounts
/// (issue #969).
fn default_horizon_url(network: &str) -> &'static str {
    if network == "public" {
        "https://horizon.stellar.org"
    } else {
        "https://horizon-testnet.stellar.org"
    }
}

/// Check that `STELLAR_NETWORK` and `STELLAR_HORIZON_URL` name the same
/// network (issue #969).
///
/// - `public` with a testnet Horizon is refused at boot: the gateway would
///   never see a real payment, and nothing else would flag it.
/// - `testnet` with the mainnet Horizon is allowed but logged as a warning,
///   since it is almost certainly a copy-paste mistake.
///
/// The check is a heuristic on the URL text. Horizon hosts name their network
/// (`horizon-testnet.stellar.org` vs `horizon.stellar.org`), so this catches
/// the standard endpoints; a custom proxy with an unrelated hostname is not
/// classified.
fn validate_network_horizon(network: &str, horizon_url: &str) -> Result<()> {
    let lower = horizon_url.to_ascii_lowercase();
    let is_testnet_horizon = lower.contains("testnet");
    let is_mainnet_horizon = lower.contains("horizon.stellar.org") && !is_testnet_horizon;

    if network == "public" && is_testnet_horizon {
        return Err(anyhow::anyhow!(
            "STELLAR_HORIZON_URL={horizon_url:?} is a testnet Horizon, but \
             STELLAR_NETWORK=public. The gateway would watch the testnet ledger for \
             mainnet accounts and never detect real payments. Set \
             STELLAR_HORIZON_URL=https://horizon.stellar.org (or your mainnet Horizon \
             endpoint), or remove it to use the mainnet default."
        ));
    }

    if network == "testnet" && is_mainnet_horizon {
        tracing::warn!(
            horizon_url = %horizon_url,
            "STELLAR_NETWORK=testnet but STELLAR_HORIZON_URL points at the mainnet \
             Horizon. Testnet accounts will not be found there; check both settings."
        );
    }

    Ok(())
}

/// Parse `ALLOWED_WEBHOOK_SCHEMES` (issue #971).
///
/// Entries are trimmed and lowercased. `reqwest::Url` always lowercases the
/// scheme it parses, so a stored `HTTPS` would reject every `https://` webhook
/// URL. Only `http` and `https` are accepted, because the SSRF guard permits
/// no other scheme and any other entry could never be used. An empty list is
/// refused, because it would reject every webhook.
fn parse_webhook_schemes(raw: &str) -> Result<Vec<String>> {
    let mut schemes: Vec<String> = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let scheme = entry.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https") {
            return Err(anyhow::anyhow!(
                "ALLOWED_WEBHOOK_SCHEMES contains an unsupported scheme {entry:?}. \
                 Only \"http\" and \"https\" are supported. Fix or remove the bad entry."
            ));
        }
        if !schemes.contains(&scheme) {
            schemes.push(scheme);
        }
    }
    if schemes.is_empty() {
        return Err(anyhow::anyhow!(
            "ALLOWED_WEBHOOK_SCHEMES is empty, so every webhook_url would be rejected. \
             Set it to \"https\", or unset it to use the default."
        ));
    }
    Ok(schemes)
}

/// Parse `TRUSTED_PROXY_CIDRS`: a comma-separated list of CIDR blocks (IPv4 or
/// IPv6), e.g. `TRUSTED_PROXY_CIDRS=10.0.0.0/8,192.168.1.0/24`. Empty/unset
/// means no trusted proxies, in which case forwarding headers are ignored
/// entirely (issue #330). A malformed entry aborts boot — a mistyped
/// allow-list must not silently degrade into trusting headers it shouldn't.
fn parse_cidrs(raw: &str) -> Result<Vec<IpNet>> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|entry| {
            entry.parse::<IpNet>().map_err(|e| {
                anyhow::anyhow!(
                    "TRUSTED_PROXY_CIDRS contains an invalid CIDR {entry:?}: {e}. \
                     Expected comma-separated CIDR blocks, e.g. 10.0.0.0/8. \
                     Fix or remove the bad entry."
                )
            })
        })
        .collect()
}

/// Parse an env var into `T`.
///
/// - If the variable is absent, `default` is returned.
/// - If the variable is present but cannot be parsed, boot is aborted with a
///   clear error message instead of silently falling back to the default.
///   This prevents misconfigured values (e.g. a typo in `PAYMENT_TTL_SECS`)
///   from going unnoticed in production.
fn parse_env<T>(key: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(raw) => raw.parse::<T>().map_err(|e| {
            anyhow::anyhow!(
                "invalid value for {key}={raw:?}: {e}. \
                 Fix the environment variable or remove it to use the default."
            )
        }),
        Err(_) => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_secrets() {
        let cfg = Config {
            port: 3000,
            bind_addr: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            database_url: "sqlite:test.db".into(),
            network: "testnet".into(),
            horizon_url: "https://horizon-testnet.stellar.org".into(),
            gateway_public: "GPUBLIC".into(),
            accepted_assets: AcceptedAsset::default_list(),
            webhook_secret: "webhook-hmac-secret".into(),
            webhook_retry_attempts: 3,
            webhook_retry_delay_ms: 5000,
            webhook_retry_max_delay_ms: 60_000,
            allowed_webhook_schemes: vec!["https".into()],
            webhook_timeout_secs: 10,
            webhook_redrive_interval_secs: 30,
            webhook_redrive_concurrency: 4,
            webhook_redrive_max_attempts: 8,
            webhook_redrive_grace_secs: 60,
            webhook_redrive_backoff_initial_secs: 30,
            webhook_redrive_backoff_max_secs: 900,
            retention_interval_secs: 3600,
            webhook_delivery_retention_days: 30,
            idempotency_retention_days: 7,
            poll_interval_secs: 10,
            expiry_sweep_interval_secs: 10,
            trustline_check_interval_secs: 3600,
            poll_max_pages_per_cycle: 50,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10,
            db_pool_max_connections: 10,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            webhook_allow_private_targets: false,
            admin_provisioning_secret: "admin-super-secret".into(),
            metrics_token: "metrics-super-secret".into(),
            request_timeout_secs: 30,
            stream_idle_timeout_secs: 30,
            trusted_proxy_cidrs: vec![],
        };
        let output = format!("{cfg:?}");
        assert!(
            !output.contains("webhook-hmac-secret"),
            "webhook_secret must not appear in Debug output"
        );
        assert!(
            !output.contains("admin-super-secret"),
            "admin_provisioning_secret must not appear in Debug output"
        );
        assert!(
            !output.contains("metrics-super-secret"),
            "metrics_token must not appear in Debug output"
        );
        assert!(
            output.contains("***"),
            "redacted marker must appear in Debug output"
        );
    }

    #[test]
    fn parse_accepted_assets_from_env_string() {
        let assets = AcceptedAsset::parse_list("XLM,USDC:GISSUER,EURC:GISSUER2");
        assert_eq!(assets.len(), 3);
        assert_eq!(
            assets[0],
            AcceptedAsset {
                code: "XLM".into(),
                issuer: None
            }
        );
        assert_eq!(
            assets[1],
            AcceptedAsset {
                code: "USDC".into(),
                issuer: Some("GISSUER".into())
            }
        );
        assert_eq!(
            assets[2],
            AcceptedAsset {
                code: "EURC".into(),
                issuer: Some("GISSUER2".into())
            }
        );
    }

    const TESTNET_USDC_ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

    #[test]
    fn default_assets_on_testnet_include_testnet_usdc() {
        let assets = AcceptedAsset::default_list_for_network("testnet");
        assert_eq!(assets.len(), 2);
        assert_eq!(assets[0].code, "XLM");
        assert_eq!(assets[0].issuer, None);
        assert_eq!(assets[1].code, "USDC");
        assert_eq!(assets[1].issuer.as_deref(), Some(TESTNET_USDC_ISSUER));
        assert_eq!(AcceptedAsset::default_list(), assets);
    }

    #[test]
    fn default_assets_on_public_are_xlm_only() {
        // Issue #968: the testnet USDC issuer must never be accepted by default
        // on mainnet, where it holds no real USDC.
        let assets = AcceptedAsset::default_list_for_network("public");
        assert_eq!(
            assets,
            vec![AcceptedAsset {
                code: "XLM".into(),
                issuer: None
            }]
        );
        assert!(
            assets
                .iter()
                .all(|a| a.issuer.as_deref() != Some(TESTNET_USDC_ISSUER)),
            "public default must not contain the testnet USDC issuer"
        );
    }

    #[test]
    fn public_network_without_accepted_assets_never_accepts_testnet_issuer() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("public")),
                ("CORS_ALLOWED_ORIGINS", Some("https://example.com")),
                ("ACCEPTED_ASSETS", None),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert!(
                    cfg.accepted_assets
                        .iter()
                        .all(|a| a.issuer.as_deref() != Some(TESTNET_USDC_ISSUER)),
                    "boot on public without ACCEPTED_ASSETS accepted the testnet issuer: {:?}",
                    cfg.accepted_assets
                );
            },
        );
    }

    // ── issue #969: STELLAR_HORIZON_URL default and network mismatch ─────────

    #[test]
    fn default_horizon_url_follows_network() {
        assert_eq!(default_horizon_url("public"), "https://horizon.stellar.org");
        assert_eq!(
            default_horizon_url("testnet"),
            "https://horizon-testnet.stellar.org"
        );
    }

    #[test]
    fn public_network_without_horizon_url_uses_mainnet_horizon() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("public")),
                ("CORS_ALLOWED_ORIGINS", Some("https://example.com")),
                ("STELLAR_HORIZON_URL", None),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.horizon_url, "https://horizon.stellar.org");
            },
        );
    }

    #[test]
    fn testnet_network_without_horizon_url_uses_testnet_horizon() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("STELLAR_HORIZON_URL", None),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.horizon_url, "https://horizon-testnet.stellar.org");
            },
        );
    }

    #[test]
    fn public_network_with_testnet_horizon_aborts_boot() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("public")),
                ("CORS_ALLOWED_ORIGINS", Some("https://example.com")),
                (
                    "STELLAR_HORIZON_URL",
                    Some("https://horizon-testnet.stellar.org"),
                ),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("STELLAR_HORIZON_URL") && err.contains("STELLAR_NETWORK=public"),
                    "error must name both settings; got: {err}"
                );
            },
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn testnet_network_with_mainnet_horizon_warns_but_boots() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("STELLAR_HORIZON_URL", Some("https://horizon.stellar.org")),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.horizon_url, "https://horizon.stellar.org");
                assert!(
                    logs_contain("STELLAR_HORIZON_URL"),
                    "testnet + mainnet Horizon must log a mismatch warning"
                );
            },
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn matching_horizon_url_does_not_warn() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                (
                    "STELLAR_HORIZON_URL",
                    Some("https://horizon-testnet.stellar.org"),
                ),
            ],
            || {
                Config::from_env().unwrap();
                assert!(!logs_contain("mainnet Horizon"));
            },
        );
    }

    // ── issue #970: CORS_ALLOWED_ORIGINS must be bare origins ────────────────

    #[test]
    fn cors_origin_bare_origin_is_accepted_unchanged() {
        assert_eq!(
            normalise_cors_origin("https://app.example.com").unwrap(),
            "https://app.example.com"
        );
    }

    #[test]
    fn cors_origin_trailing_slash_is_stripped() {
        assert_eq!(
            normalise_cors_origin("https://app.example.com/").unwrap(),
            "https://app.example.com"
        );
    }

    #[test]
    fn cors_origin_keeps_non_default_port() {
        assert_eq!(
            normalise_cors_origin("http://localhost:3001/").unwrap(),
            "http://localhost:3001"
        );
    }

    #[test]
    fn cors_origin_is_normalised_to_browser_origin_form() {
        // Browsers send lowercase scheme/host and omit the default port.
        assert_eq!(
            normalise_cors_origin("HTTPS://App.Example.com:443/").unwrap(),
            "https://app.example.com"
        );
    }

    #[test]
    fn cors_origin_with_path_is_rejected() {
        let err = normalise_cors_origin("https://app.example.com/checkout")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("https://app.example.com/checkout"),
            "got: {err}"
        );
        assert!(err.contains("path"), "got: {err}");
    }

    #[test]
    fn cors_origin_without_scheme_is_rejected() {
        let err = normalise_cors_origin("app.example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("app.example.com"), "got: {err}");
    }

    #[test]
    fn cors_origin_wildcard_is_rejected() {
        let err = normalise_cors_origin("*").unwrap_err().to_string();
        assert!(err.contains("wildcard"), "got: {err}");
    }

    #[test]
    fn cors_origin_non_http_scheme_is_rejected() {
        assert!(normalise_cors_origin("ftp://app.example.com").is_err());
        assert!(normalise_cors_origin("ws://app.example.com").is_err());
    }

    #[test]
    fn cors_origin_double_trailing_slash_is_rejected() {
        assert!(normalise_cors_origin("https://app.example.com//").is_err());
    }

    #[test]
    fn cors_origin_query_and_fragment_are_rejected() {
        assert!(normalise_cors_origin("https://app.example.com?x=1").is_err());
        assert!(normalise_cors_origin("https://app.example.com#top").is_err());
        assert!(normalise_cors_origin("https://app.example.com/?").is_err());
    }

    #[test]
    fn cors_origin_with_credentials_is_rejected() {
        assert!(normalise_cors_origin("https://user:pass@app.example.com").is_err());
    }

    #[test]
    fn cors_origin_without_host_is_rejected() {
        assert!(normalise_cors_origin("https://").is_err());
    }

    #[test]
    fn startup_normalises_cors_origins_with_trailing_slash() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("public")),
                ("CORS_ALLOWED_ORIGINS", Some("https://app.example.com/")),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.cors_allowed_origins, vec!["https://app.example.com"]);
            },
        );
    }

    #[test]
    fn startup_aborts_on_cors_origin_with_path() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("public")),
                (
                    "CORS_ALLOWED_ORIGINS",
                    Some("https://app.example.com,https://app.example.com/checkout"),
                ),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(err.contains("CORS_ALLOWED_ORIGINS"), "got: {err}");
                assert!(err.contains("/checkout"), "got: {err}");
            },
        );
    }

    #[test]
    fn startup_aborts_on_wildcard_cors_origin() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("CORS_ALLOWED_ORIGINS", Some("*")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(err.contains("CORS_ALLOWED_ORIGINS"), "got: {err}");
            },
        );
    }

    // ── issue #971: ALLOWED_WEBHOOK_SCHEMES normalisation and validation ─────

    #[test]
    fn webhook_schemes_are_lowercased() {
        assert_eq!(parse_webhook_schemes("HTTPS").unwrap(), vec!["https"]);
        assert_eq!(
            parse_webhook_schemes("https,HTTP").unwrap(),
            vec!["https", "http"]
        );
    }

    #[test]
    fn webhook_schemes_trim_and_deduplicate() {
        assert_eq!(
            parse_webhook_schemes(" https , https ,, ").unwrap(),
            vec!["https"]
        );
    }

    #[test]
    fn webhook_schemes_reject_unsupported_scheme() {
        for raw in ["ftp", "ws", "https,ftp", "file"] {
            let err = parse_webhook_schemes(raw).unwrap_err().to_string();
            assert!(
                err.contains("unsupported scheme") && err.contains("ALLOWED_WEBHOOK_SCHEMES"),
                "{raw:?} should be rejected; got: {err}"
            );
        }
    }

    #[test]
    fn webhook_schemes_reject_empty_list() {
        for raw in [",", "", "  ", " , , "] {
            let err = parse_webhook_schemes(raw).unwrap_err().to_string();
            assert!(
                err.contains("is empty"),
                "{raw:?} should be rejected; got: {err}"
            );
        }
    }

    #[test]
    fn startup_accepts_upper_case_https_webhook_scheme() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("ALLOWED_WEBHOOK_SCHEMES", Some("HTTPS")),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.allowed_webhook_schemes, vec!["https"]);
            },
        );
    }

    #[test]
    fn startup_aborts_on_unsupported_webhook_scheme() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("ALLOWED_WEBHOOK_SCHEMES", Some("ftp")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(err.contains("ftp"), "got: {err}");
                assert!(err.contains("ALLOWED_WEBHOOK_SCHEMES"), "got: {err}");
            },
        );
    }

    #[test]
    fn startup_aborts_on_empty_webhook_scheme_list() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("ALLOWED_WEBHOOK_SCHEMES", Some(",")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(err.contains("ALLOWED_WEBHOOK_SCHEMES"), "got: {err}");
            },
        );
    }

    #[test]
    fn webhook_schemes_default_to_https_when_unset() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("ALLOWED_WEBHOOK_SCHEMES", None),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.allowed_webhook_schemes, vec!["https"]);
            },
        );
    }

    fn sample_config() -> Config {
        Config {
            port: 3000,
            bind_addr: std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            database_url: "sqlite::memory:".into(),
            network: "testnet".into(),
            horizon_url: "https://horizon-testnet.stellar.org".into(),
            gateway_public: "UNCONFIGURED".into(),
            accepted_assets: AcceptedAsset::default_list(),
            webhook_secret: String::new(),
            webhook_retry_attempts: 3,
            webhook_retry_delay_ms: 5000,
            webhook_retry_max_delay_ms: 60_000,
            allowed_webhook_schemes: vec!["https".into()],
            webhook_timeout_secs: 10,
            webhook_redrive_interval_secs: 30,
            webhook_redrive_concurrency: 4,
            webhook_redrive_max_attempts: 8,
            webhook_redrive_grace_secs: 60,
            webhook_redrive_backoff_initial_secs: 30,
            webhook_redrive_backoff_max_secs: 900,
            retention_interval_secs: 3600,
            webhook_delivery_retention_days: 30,
            idempotency_retention_days: 7,
            poll_interval_secs: 10,
            expiry_sweep_interval_secs: 10,
            trustline_check_interval_secs: 3600,
            poll_max_pages_per_cycle: 50,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10,
            db_pool_max_connections: 10,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            webhook_allow_private_targets: false,
            admin_provisioning_secret: String::new(),
            metrics_token: String::new(),
            request_timeout_secs: 30,
            stream_idle_timeout_secs: 30,
            trusted_proxy_cidrs: vec![],
        }
    }

    #[test]
    fn validate_addresses_passes_for_unconfigured_gateway_and_default_issuer() {
        // The placeholder gateway is skipped; the default USDC issuer is valid.
        assert!(sample_config().validate_addresses().is_ok());
    }

    #[test]
    fn validate_addresses_accepts_a_real_gateway_key() {
        let mut cfg = sample_config();
        cfg.gateway_public = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into();
        assert!(cfg.validate_addresses().is_ok());
    }

    #[test]
    fn validate_addresses_rejects_a_corrupted_gateway_key() {
        let mut cfg = sample_config();
        // A valid key with one character flipped — a realistic typo.
        cfg.gateway_public = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLB5".into();
        let err = cfg.validate_addresses().unwrap_err().to_string();
        assert!(err.contains("STELLAR_GATEWAY_PUBLIC"), "got: {err}");
    }

    #[test]
    fn validate_addresses_rejects_an_invalid_issuer() {
        let mut cfg = sample_config();
        cfg.accepted_assets = vec![AcceptedAsset {
            code: "USDC".into(),
            issuer: Some("GNOTAREALISSUER".into()),
        }];
        let err = cfg.validate_addresses().unwrap_err().to_string();
        assert!(err.contains("USDC"), "got: {err}");
    }

    #[test]
    fn validate_webhook_secret_missing() {
        let err = Config::validate_webhook_secret(Err(std::env::VarError::NotPresent))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("environment variable is missing"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_webhook_secret_empty() {
        let err = Config::validate_webhook_secret(Ok("".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot be empty"), "got: {err}");
    }

    #[test]
    fn validate_webhook_secret_whitespace() {
        let err = Config::validate_webhook_secret(Ok("   ".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot contain only whitespace"), "got: {err}");
    }

    #[test]
    fn validate_webhook_secret_default() {
        let err = Config::validate_webhook_secret(Ok("default-secret".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("known placeholder value"), "got: {err}");
    }

    #[test]
    fn validate_webhook_secret_short() {
        let err = Config::validate_webhook_secret(Ok("too-short".into()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("must be at least 32 characters long"),
            "got: {err}"
        );
    }

    #[test]
    fn validate_webhook_secret_valid() {
        let secret = "a-very-long-and-secure-webhook-signing-secret-32-chars";
        let res = Config::validate_webhook_secret(Ok(secret.into()))
            .expect("valid webhook secret should be accepted");
        assert_eq!(res, secret);
    }

    fn run_with_env<F>(env_vars: &[(&str, Option<&str>)], f: F)
    where
        F: FnOnce(),
    {
        use std::sync::OnceLock;
        static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
        let _guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .expect("env lock should not be poisoned");

        // Backup current env values
        let backups: Vec<(String, Option<String>)> = env_vars
            .iter()
            .map(|(key, _)| (key.to_string(), std::env::var(key).ok()))
            .collect();

        // Set new values
        for &(key, val) in env_vars {
            if let Some(v) = val {
                // SAFETY: test-only; callers serialise env access behind a lock.
                unsafe { std::env::set_var(key, v) };
            } else {
                // SAFETY: test-only; callers serialise env access behind a lock.
                unsafe { std::env::remove_var(key) };
            }
        }

        // Run the test logic
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        // Restore backups
        for (key, val) in backups {
            if let Some(v) = val {
                // SAFETY: test-only; callers serialise env access behind a lock.
                unsafe { std::env::set_var(key, v) };
            } else {
                // SAFETY: test-only; callers serialise env access behind a lock.
                unsafe { std::env::remove_var(key) };
            }
        }

        if let Err(err) = res {
            std::panic::resume_unwind(err);
        }
    }

    #[test]
    fn startup_fails_in_production_if_webhook_secret_missing() {
        run_with_env(
            &[
                ("STELLAR_NETWORK", Some("public")),
                ("WEBHOOK_SECRET", None),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("WEBHOOK_SECRET environment variable is missing"),
                    "got: {err}"
                );
            },
        );
    }

    #[test]
    fn startup_succeeds_with_valid_configuration() {
        run_with_env(
            &[
                ("STELLAR_NETWORK", Some("public")),
                (
                    "WEBHOOK_SECRET",
                    Some("a-very-long-and-secure-webhook-signing-secret-32-chars"),
                ),
                ("DATABASE_URL", Some("sqlite::memory:")),
                (
                    "STELLAR_GATEWAY_PUBLIC",
                    Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5"),
                ),
                (
                    "STELLAR_GATEWAY_SECRET",
                    Some("SCZANGBA5RLKJHTBF4RJNRJMZWI4VKTHCRKOVAH7LRZZPZHHZWATAWBN"),
                ),
                ("CORS_ALLOWED_ORIGINS", Some("https://example.com")),
            ],
            || {
                let cfg = Config::from_env()
                    .expect("Config::from_env should succeed with CORS_ALLOWED_ORIGINS set");
                assert_eq!(cfg.network, "public");
                assert_eq!(
                    cfg.webhook_secret,
                    "a-very-long-and-secure-webhook-signing-secret-32-chars"
                );
                assert_eq!(cfg.cors_allowed_origins, vec!["https://example.com"]);
            },
        );
    }

    #[test]
    fn startup_fails_on_public_without_cors_allowed_origins() {
        run_with_env(
            &[
                ("STELLAR_NETWORK", Some("public")),
                (
                    "WEBHOOK_SECRET",
                    Some("a-very-long-and-secure-webhook-signing-secret-32-chars"),
                ),
                ("DATABASE_URL", Some("sqlite::memory:")),
                (
                    "STELLAR_GATEWAY_PUBLIC",
                    Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5"),
                ),
                (
                    "STELLAR_GATEWAY_SECRET",
                    Some("SCZANGBA5RLKJHTBF4RJNRJMZWI4VKTHCRKOVAH7LRZZPZHHZWATAWBN"),
                ),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("CORS_ALLOWED_ORIGINS must be set")
                        || err.contains(
                            "CORS_ALLOWED_ORIGINS must be set when STELLAR_NETWORK=public"
                        ),
                    "got: {err}"
                );
            },
        );
    }

    // ── validate_timing ──────────────────────────────────────────────────────

    fn timing_config() -> Config {
        let mut cfg = sample_config();
        cfg.poll_interval_secs = 10;
        cfg.payment_ttl_secs = 3600;
        cfg.webhook_retry_attempts = 3;
        cfg.webhook_retry_delay_ms = 5000;
        cfg
    }

    #[test]
    fn timing_valid_defaults_pass() {
        assert!(timing_config().validate_timing().is_ok());
    }

    // ── Retry schedule and grace-window validation (issues #318, #238) ───────

    /// The bound the grace window is checked against: every attempt may burn a
    /// full timeout, and the gaps follow the exponential schedule.
    #[test]
    fn worst_case_inline_sums_the_exponential_schedule() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 3;
        cfg.webhook_timeout_secs = 10;
        cfg.webhook_retry_delay_ms = 5_000;
        cfg.webhook_retry_max_delay_ms = 60_000;
        // 3 × 10s of timeouts, plus gaps of 5s and 10s.
        assert_eq!(cfg.worst_case_inline_delivery_secs(), 45);
    }

    /// The cap has to actually bind, or a long retry chain would report an
    /// absurd worst case and demand an equally absurd grace window.
    #[test]
    fn worst_case_inline_respects_the_delay_cap() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 5;
        cfg.webhook_timeout_secs = 1;
        cfg.webhook_retry_delay_ms = 1_000;
        cfg.webhook_retry_max_delay_ms = 2_000;
        // 5 × 1s, plus gaps of 1s, 2s, 2s (capped), 2s (capped).
        assert_eq!(cfg.worst_case_inline_delivery_secs(), 12);
    }

    /// A single attempt has no gaps at all.
    #[test]
    fn worst_case_inline_with_no_retries_is_just_one_timeout() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 1;
        cfg.webhook_timeout_secs = 10;
        assert_eq!(cfg.worst_case_inline_delivery_secs(), 10);
    }

    /// The failure this guards against is a duplicate delivery: the worker
    /// picking up a row whose inline dispatch has not finished.
    #[test]
    fn timing_rejects_a_grace_window_shorter_than_the_inline_schedule() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 5;
        cfg.webhook_timeout_secs = 30;
        cfg.webhook_retry_delay_ms = 5_000;
        cfg.webhook_retry_max_delay_ms = 60_000;
        cfg.webhook_redrive_grace_secs = 60; // far below 150s of timeouts alone
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(
            err.contains("WEBHOOK_REDRIVE_GRACE_SECS") && err.contains("send it twice"),
            "got: {err}"
        );
    }

    #[test]
    fn timing_rejects_a_retry_cap_below_the_base_delay() {
        let mut cfg = timing_config();
        cfg.webhook_retry_delay_ms = 5_000;
        cfg.webhook_retry_max_delay_ms = 1_000;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("WEBHOOK_RETRY_MAX_DELAY_MS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_zero_poll_interval() {
        let mut cfg = timing_config();
        cfg.poll_interval_secs = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("POLL_INTERVAL_SECS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_zero_stream_idle_timeout() {
        let mut cfg = timing_config();
        cfg.stream_idle_timeout_secs = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("STREAM_IDLE_TIMEOUT_SECS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_redrive_cap_equal_to_inline_attempts() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 3;
        cfg.webhook_redrive_max_attempts = 3;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(
            err.contains("WEBHOOK_REDRIVE_MAX_ATTEMPTS") && err.contains("WEBHOOK_RETRY_ATTEMPTS"),
            "got: {err}"
        );
    }

    #[test]
    fn timing_rejects_redrive_cap_below_inline_attempts() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 5;
        cfg.webhook_redrive_max_attempts = 3;
        assert!(cfg.validate_timing().is_err());
    }

    #[test]
    fn timing_allows_redrive_cap_one_above_inline_attempts() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 3;
        cfg.webhook_redrive_max_attempts = 4;
        assert!(cfg.validate_timing().is_ok());
    }

    #[test]
    fn timing_rejects_zero_webhook_timeout() {
        let mut cfg = timing_config();
        cfg.webhook_timeout_secs = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("WEBHOOK_TIMEOUT_SECS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_zero_db_busy_timeout() {
        let mut cfg = timing_config();
        cfg.db_busy_timeout_ms = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("DB_BUSY_TIMEOUT_MS"), "got: {err}");
    }

    #[test]
    fn timing_allows_nonzero_db_busy_timeout() {
        let mut cfg = timing_config();
        cfg.db_busy_timeout_ms = 1;
        assert!(cfg.validate_timing().is_ok());
    }

    #[test]
    fn timing_rejects_zero_ttl() {
        let mut cfg = timing_config();
        cfg.payment_ttl_secs = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("PAYMENT_TTL_SECS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_ttl_shorter_than_poll_interval() {
        let mut cfg = timing_config();
        cfg.poll_interval_secs = 60;
        cfg.payment_ttl_secs = 30; // < poll interval
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(
            err.contains("PAYMENT_TTL_SECS") && err.contains("POLL_INTERVAL_SECS"),
            "got: {err}"
        );
    }

    #[test]
    fn timing_allows_ttl_equal_to_poll_interval() {
        let mut cfg = timing_config();
        cfg.poll_interval_secs = 60;
        cfg.payment_ttl_secs = 60; // equal is fine
        assert!(cfg.validate_timing().is_ok());
    }

    #[test]
    fn timing_rejects_zero_retry_attempts() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("WEBHOOK_RETRY_ATTEMPTS"), "got: {err}");
    }

    #[test]
    fn timing_rejects_zero_delay_with_multiple_retries() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 3;
        cfg.webhook_retry_delay_ms = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("WEBHOOK_RETRY_DELAY_MS"), "got: {err}");
    }

    #[test]
    fn timing_allows_zero_delay_with_single_attempt() {
        let mut cfg = timing_config();
        cfg.webhook_retry_attempts = 1;
        cfg.webhook_retry_delay_ms = 0; // no retries, so no burst
        assert!(cfg.validate_timing().is_ok());
    }

    #[test]
    fn timing_rejects_backoff_max_below_initial() {
        let mut cfg = timing_config();
        cfg.webhook_redrive_backoff_initial_secs = 300;
        cfg.webhook_redrive_backoff_max_secs = 30;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(
            err.contains("WEBHOOK_REDRIVE_BACKOFF_MAX_SECS")
                && err.contains("WEBHOOK_REDRIVE_BACKOFF_INITIAL_SECS"),
            "got: {err}"
        );
    }

    #[test]
    fn timing_allows_backoff_max_equal_to_initial() {
        let mut cfg = timing_config();
        cfg.webhook_redrive_backoff_initial_secs = 30;
        cfg.webhook_redrive_backoff_max_secs = 30;
        assert!(cfg.validate_timing().is_ok());
    }

    #[test]
    fn timing_allows_zero_backoff_initial_to_disable_growth() {
        let mut cfg = timing_config();
        cfg.webhook_redrive_backoff_initial_secs = 0;
        cfg.webhook_redrive_backoff_max_secs = 0;
        assert!(cfg.validate_timing().is_ok());
    }

    /// Each of the five `i64` settings must reject a negative value and name
    /// the variable in the error, so the operator knows what to fix.
    #[test]
    fn timing_rejects_negative_i64_settings_naming_the_variable() {
        type Setter = fn(&mut Config, i64);
        let cases: [(&str, Setter); 5] = [
            ("WEBHOOK_DELIVERY_RETENTION_DAYS", |c, v| {
                c.webhook_delivery_retention_days = v
            }),
            ("IDEMPOTENCY_RETENTION_DAYS", |c, v| {
                c.idempotency_retention_days = v
            }),
            ("WEBHOOK_REDRIVE_GRACE_SECS", |c, v| {
                c.webhook_redrive_grace_secs = v
            }),
            ("WEBHOOK_REDRIVE_BACKOFF_INITIAL_SECS", |c, v| {
                c.webhook_redrive_backoff_initial_secs = v
            }),
            ("WEBHOOK_REDRIVE_BACKOFF_MAX_SECS", |c, v| {
                c.webhook_redrive_backoff_max_secs = v
            }),
        ];
        for (name, set) in cases {
            let mut cfg = timing_config();
            set(&mut cfg, -5);
            let err = cfg.validate_timing().unwrap_err().to_string();
            assert!(err.contains(name), "{name}: got: {err}");
        }
    }

    /// Zero keeps its documented meaning for the retention windows (disabled).
    #[test]
    fn timing_allows_zero_retention_days_to_disable_pruning() {
        let mut cfg = timing_config();
        cfg.webhook_delivery_retention_days = 0;
        cfg.idempotency_retention_days = 0;
        assert!(cfg.validate_timing().is_ok());
    }

    #[test]
    fn startup_fails_on_ttl_shorter_than_poll_interval_via_env() {
        run_with_env(
            &[
                (
                    "WEBHOOK_SECRET",
                    Some("a-very-long-and-secure-webhook-signing-secret-32-chars"),
                ),
                ("POLL_INTERVAL_SECS", Some("300")),
                ("PAYMENT_TTL_SECS", Some("60")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("PAYMENT_TTL_SECS") || err.contains("POLL_INTERVAL_SECS"),
                    "got: {err}"
                );
            },
        );
    }

    // ── ListenerMode::parse ──────────────────────────────────────────────────

    #[test]
    fn listener_mode_empty_defaults_to_stream() {
        assert_eq!(
            ListenerMode::parse("").expect("ListenerMode::parse should accept an empty value"),
            ListenerMode::Stream
        );
    }

    #[test]
    fn listener_mode_stream_parses() {
        assert_eq!(
            ListenerMode::parse("stream").expect("ListenerMode::parse should accept stream"),
            ListenerMode::Stream
        );
        assert_eq!(
            ListenerMode::parse("STREAM")
                .expect("ListenerMode::parse should accept STREAM case-insensitively"),
            ListenerMode::Stream
        );
    }

    #[test]
    fn listener_mode_poll_parses() {
        assert_eq!(
            ListenerMode::parse("poll").expect("ListenerMode::parse should accept poll"),
            ListenerMode::Poll
        );
        assert_eq!(
            ListenerMode::parse("POLL")
                .expect("ListenerMode::parse should accept POLL case-insensitively"),
            ListenerMode::Poll
        );
    }

    #[test]
    fn listener_mode_invalid_aborts_boot() {
        let err = ListenerMode::parse("streem").unwrap_err().to_string();
        assert!(
            err.contains("STELLAR_LISTENER_MODE"),
            "error should name the variable; got: {err}"
        );
        assert!(
            err.contains("streem"),
            "error should echo the bad value; got: {err}"
        );
    }

    // ── BIND_ADDR (issue #967) ───────────────────────────────────────────────

    #[test]
    fn bind_addr_defaults_to_all_ipv4_interfaces() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("BIND_ADDR", None),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(
                    cfg.bind_addr,
                    std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
                );
            },
        );
    }

    #[test]
    fn bind_addr_accepts_loopback_and_ipv6() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("BIND_ADDR", Some("127.0.0.1")),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(
                    cfg.bind_addr,
                    "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
                );
            },
        );
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("BIND_ADDR", Some("::")),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert!(cfg.bind_addr.is_ipv6(), "BIND_ADDR=:: must select IPv6");
            },
        );
    }

    #[test]
    fn bind_addr_invalid_value_aborts_boot() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("BIND_ADDR", Some("localhost")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("BIND_ADDR"),
                    "error should name the variable; got: {err}"
                );
                assert!(
                    err.contains("localhost"),
                    "error should echo the value; got: {err}"
                );
            },
        );
    }

    // ── CURSOR_STALENESS_MULTIPLE ────────────────────────────────────────────

    /// Every `run_with_env` closure below must set a valid WEBHOOK_SECRET (and
    /// anything else `from_env` hard-requires), otherwise the panic inside the
    /// closure poisons the shared env-test mutex and every subsequent
    /// `run_with_env` test fails at the lock.
    const ENV_WEBHOOK_SECRET: &str = "a-very-long-and-secure-webhook-signing-secret-32-chars";

    // ── Plaintext webhook scheme startup warning (issue #306) ────────────────

    #[test]
    #[tracing_test::traced_test]
    fn allowing_http_webhook_scheme_warns_at_boot_on_any_network() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("ALLOWED_WEBHOOK_SCHEMES", Some("https,http")),
            ],
            || {
                Config::from_env()
                    .expect("Config::from_env should succeed with https,http webhook schemes");
                assert!(
                    logs_contain("ALLOWED_WEBHOOK_SCHEMES"),
                    "including http in ALLOWED_WEBHOOK_SCHEMES must log a warning, \
                     even on a non-public network"
                );
            },
        );
    }

    #[test]
    #[tracing_test::traced_test]
    fn https_only_webhook_schemes_do_not_warn() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("ALLOWED_WEBHOOK_SCHEMES", Some("https")),
            ],
            || {
                Config::from_env()
                    .expect("Config::from_env should succeed with the https webhook scheme");
                assert!(!logs_contain("ALLOWED_WEBHOOK_SCHEMES"));
            },
        );
    }

    // ── issue #246: WEBHOOK_ALLOW_PRIVATE_TARGETS on public network ───────────

    /// The flag must be rejected at boot when STELLAR_NETWORK=public (issue #246).
    #[test]
    fn private_targets_on_public_network_aborts_boot() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("public")),
                ("CORS_ALLOWED_ORIGINS", Some("https://example.com")),
                ("WEBHOOK_ALLOW_PRIVATE_TARGETS", Some("true")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("WEBHOOK_ALLOW_PRIVATE_TARGETS"),
                    "error must name the offending variable; got: {err}"
                );
                assert!(
                    err.contains("STELLAR_NETWORK=public"),
                    "error must mention the network constraint; got: {err}"
                );
            },
        );
    }

    /// The flag is allowed on testnet (development use-case). Boot must succeed.
    #[test]
    fn private_targets_on_testnet_is_allowed() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("STELLAR_NETWORK", Some("testnet")),
                ("WEBHOOK_ALLOW_PRIVATE_TARGETS", Some("true")),
            ],
            || {
                let cfg = Config::from_env().expect(
                    "Config::from_env should succeed on testnet with private targets allowed",
                );
                assert!(
                    cfg.webhook_allow_private_targets,
                    "flag should be set on testnet"
                );
            },
        );
    }

    // ── issue #245: ADMIN_PROVISIONING_SECRET validation ──────────────────────

    /// An empty (unset) admin secret disables provisioning — boot must succeed.
    #[test]
    fn admin_secret_empty_disables_provisioning() {
        let result = Config::validate_admin_secret(Err(std::env::VarError::NotPresent));
        assert!(
            result.is_ok(),
            "absent secret should succeed; got: {:?}",
            result
        );
        assert_eq!(
            result.expect("validate_admin_secret should accept an absent secret"),
            ""
        );
    }

    /// An explicitly empty env var is the same as absent.
    #[test]
    fn admin_secret_explicit_empty_string_disables_provisioning() {
        let result = Config::validate_admin_secret(Ok(String::new()));
        assert!(
            result.is_ok(),
            "empty string should succeed; got: {:?}",
            result
        );
        assert_eq!(
            result.expect("validate_admin_secret should accept an empty secret"),
            ""
        );
    }

    /// A value shorter than 32 characters is rejected.
    #[test]
    fn admin_secret_too_short_aborts_boot() {
        let result = Config::validate_admin_secret(Ok("short".into()));
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("at least 32 characters"),
            "error must mention the length requirement; got: {err}"
        );
    }

    /// Exactly 31 characters is one under the limit.
    #[test]
    fn admin_secret_31_chars_is_rejected() {
        let result = Config::validate_admin_secret(Ok("a".repeat(31)));
        assert!(
            result.is_err(),
            "31-char secret should be rejected; got: {:?}",
            result
        );
    }

    /// Exactly 32 characters is the boundary — must be accepted.
    #[test]
    fn admin_secret_32_chars_is_accepted() {
        let result = Config::validate_admin_secret(Ok("a".repeat(32)));
        assert!(
            result.is_ok(),
            "32-char secret should be accepted; got: {:?}",
            result
        );
    }

    /// Known placeholder values are rejected regardless of length.
    #[test]
    fn admin_secret_placeholder_admin_is_rejected() {
        let result = Config::validate_admin_secret(Ok("admin".into()));
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("placeholder"),
            "error must mention placeholder; got: {err}"
        );
    }

    #[test]
    fn admin_secret_placeholder_changeme_is_rejected() {
        let result = Config::validate_admin_secret(Ok("changeme".into()));
        assert!(result.is_err(), "changeme should be rejected");
    }

    #[test]
    fn admin_secret_placeholder_replace_me_prefix_is_rejected() {
        let result =
            Config::validate_admin_secret(Ok("REPLACE_ME_admin_provisioning_secret".into()));
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("placeholder"),
            "error must mention placeholder; got: {err}"
        );
    }

    /// A strong, randomly-generated secret must be accepted.
    #[test]
    fn admin_secret_strong_value_is_accepted() {
        let strong = "a3f8b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1";
        let result = Config::validate_admin_secret(Ok(strong.into()));
        assert!(
            result.is_ok(),
            "strong secret should be accepted; got: {:?}",
            result
        );
        assert_eq!(
            result.expect("validate_admin_secret should accept a strong secret"),
            strong
        );
    }

    /// The whole from_env path must accept a strong admin secret.
    #[test]
    fn admin_secret_from_env_strong_value_is_accepted() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                (
                    "ADMIN_PROVISIONING_SECRET",
                    Some("a3f8b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"),
                ),
            ],
            || {
                let cfg = Config::from_env()
                    .expect("Config::from_env should succeed with ADMIN_PROVISIONING_SECRET set");
                assert!(!cfg.admin_provisioning_secret.is_empty());
            },
        );
    }

    /// A short admin secret must abort boot through from_env.
    #[test]
    fn admin_secret_from_env_short_value_aborts_boot() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("ADMIN_PROVISIONING_SECRET", Some("tooshort")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("ADMIN_PROVISIONING_SECRET")
                        || err.contains("at least 32 characters"),
                    "error must indicate the secret is too short; got: {err}"
                );
            },
        );
    }

    // ── issue #250: METRICS_TOKEN validation ───────────────────────────────────

    /// An empty (unset) metrics token disables scraping — boot must succeed.
    #[test]
    fn metrics_token_empty_disables_scraping() {
        let result = Config::validate_metrics_token(Err(std::env::VarError::NotPresent));
        assert!(
            result.is_ok(),
            "absent token should succeed; got: {:?}",
            result
        );
        assert_eq!(
            result.expect("validate_metrics_token should accept an absent token"),
            ""
        );
    }

    /// An explicitly empty env var is the same as absent.
    #[test]
    fn metrics_token_explicit_empty_string_disables_scraping() {
        let result = Config::validate_metrics_token(Ok(String::new()));
        assert!(
            result.is_ok(),
            "empty string should succeed; got: {:?}",
            result
        );
        assert_eq!(
            result.expect("validate_metrics_token should accept an empty token"),
            ""
        );
    }

    /// A value shorter than 32 characters is rejected.
    #[test]
    fn metrics_token_too_short_aborts_boot() {
        let result = Config::validate_metrics_token(Ok("short".into()));
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("at least 32 characters"),
            "error must mention the length requirement; got: {err}"
        );
    }

    /// Exactly 32 characters is the boundary — must be accepted.
    #[test]
    fn metrics_token_32_chars_is_accepted() {
        let result = Config::validate_metrics_token(Ok("a".repeat(32)));
        assert!(
            result.is_ok(),
            "32-char token should be accepted; got: {:?}",
            result
        );
    }

    /// Known placeholder values are rejected regardless of length.
    #[test]
    fn metrics_token_placeholder_is_rejected() {
        let result = Config::validate_metrics_token(Ok("changeme".into()));
        assert!(result.is_err(), "changeme should be rejected");
    }

    /// A strong, randomly-generated token must be accepted.
    #[test]
    fn metrics_token_strong_value_is_accepted() {
        let strong = "a3f8b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1";
        let result = Config::validate_metrics_token(Ok(strong.into()));
        assert!(
            result.is_ok(),
            "strong token should be accepted; got: {:?}",
            result
        );
        assert_eq!(
            result.expect("validate_metrics_token should accept a strong token"),
            strong
        );
    }

    /// The whole from_env path must accept a strong metrics token.
    #[test]
    fn metrics_token_from_env_strong_value_is_accepted() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                (
                    "METRICS_TOKEN",
                    Some("a3f8b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1"),
                ),
            ],
            || {
                let cfg = Config::from_env()
                    .expect("Config::from_env should succeed with METRICS_TOKEN set");
                assert!(!cfg.metrics_token.is_empty());
            },
        );
    }

    /// A short metrics token must abort boot through from_env.
    #[test]
    fn metrics_token_from_env_short_value_aborts_boot() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("METRICS_TOKEN", Some("tooshort")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("METRICS_TOKEN") || err.contains("at least 32 characters"),
                    "error must indicate the token is too short; got: {err}"
                );
            },
        );
    }

    /// With no METRICS_TOKEN set, from_env boots successfully and the
    /// resulting config disables scraping (acceptance criterion: /metrics is
    /// not reachable anonymously from the public listener by default).
    #[test]
    fn metrics_token_unset_by_default() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("METRICS_TOKEN", None),
            ],
            || {
                let cfg = Config::from_env()
                    .expect("Config::from_env should succeed with METRICS_TOKEN unset");
                assert!(
                    cfg.metrics_token.is_empty(),
                    "metrics_token should be empty (disabled) by default"
                );
            },
        );
    }
}
