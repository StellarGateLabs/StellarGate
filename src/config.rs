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

    pub fn default_list() -> Vec<Self> {
        vec![
            AcceptedAsset {
                code: "XLM".into(),
                issuer: None,
            },
            AcceptedAsset {
                code: "USDC".into(),
                issuer: Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into()),
            },
        ]
    }
}

#[derive(Clone)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub network: String,
    pub horizon_url: String,
    pub gateway_public: String,
    /// Assets the gateway will accept, validated on POST /payments and in verify().
    /// Configure via ACCEPTED_ASSETS=XLM,USDC:GISSUER (comma-separated).
    pub accepted_assets: Vec<AcceptedAsset>,
    pub webhook_secret: String,
    pub webhook_retry_attempts: u32,
    pub webhook_retry_delay_ms: u64,
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
    /// Per-request timeout for the whole API, in seconds. A request whose
    /// handler hasn't produced a response within this window is aborted with
    /// `408 Request Timeout`, so a slow client or a stuck handler can't tie up
    /// a connection indefinitely. Defaults to 30 seconds.
    pub request_timeout_secs: u64,
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
            .unwrap_or_else(|_| "https://horizon-testnet.stellar.org".to_string());
        let gateway_public =
            std::env::var("STELLAR_GATEWAY_PUBLIC").unwrap_or_else(|_| "UNCONFIGURED".to_string());
        let webhook_secret = Self::validate_webhook_secret(std::env::var("WEBHOOK_SECRET"))?;
        let allowed_webhook_schemes: Vec<String> = {
            let raw_schemes =
                std::env::var("ALLOWED_WEBHOOK_SCHEMES").unwrap_or_else(|_| "https".to_string());
            raw_schemes
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };

        let cors_allowed_origins: Vec<String> = {
            let raw_origins: Vec<String> = std::env::var("CORS_ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();

            // Validate every configured origin now so a typo aborts boot with
            // a clear message instead of silently removing the origin from the
            // allowlist (or producing an empty allowlist with no error).
            for origin in &raw_origins {
                origin.parse::<axum::http::HeaderValue>().map_err(|e| {
                    anyhow::anyhow!(
                        "CORS_ALLOWED_ORIGINS contains an invalid origin {origin:?}: {e}. \
                         Fix or remove the bad entry."
                    )
                })?;
            }
            raw_origins
        };

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

        let config = Self {
            port: parse_env("PORT", 3000)?,
            database_url,
            network,
            horizon_url,
            gateway_public,
            accepted_assets: {
                let raw = std::env::var("ACCEPTED_ASSETS").unwrap_or_default();
                if raw.is_empty() {
                    AcceptedAsset::default_list()
                } else {
                    AcceptedAsset::parse_list(&raw)
                }
            },
            webhook_secret,
            allowed_webhook_schemes,
            webhook_retry_attempts: parse_env("WEBHOOK_RETRY_ATTEMPTS", 3)?,
            webhook_retry_delay_ms: parse_env("WEBHOOK_RETRY_DELAY_MS", 5000)?,
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
            poll_interval_secs: parse_env("POLL_INTERVAL_SECS", 10)?,
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
            request_timeout_secs: parse_env("REQUEST_TIMEOUT_SECS", 30)?,
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
    /// - `WEBHOOK_REDRIVE_BACKOFF_MAX_SECS < WEBHOOK_REDRIVE_BACKOFF_INITIAL_SECS`
    ///   → the cap would silently override the starting delay, so backoff
    ///   never actually grows
    fn validate_timing(&self) -> Result<()> {
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

        if self.request_timeout_secs == 0 {
            return Err(anyhow::anyhow!(
                "REQUEST_TIMEOUT_SECS must be > 0 (got 0). \
                 A zero timeout would abort every request immediately."
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

    fn validate_webhook_secret(raw_secret: Result<String, std::env::VarError>) -> Result<String> {
        let secret = match raw_secret {
            Ok(s) => s,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "WEBHOOK_SECRET environment variable is missing"
                ))
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
        let secret = match raw_secret {
            Ok(s) => s,
            // Absent is treated the same as empty: provisioning disabled.
            Err(_) => String::new(),
        };

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
            || secret.to_ascii_lowercase() == "admin"
            || secret.to_ascii_lowercase() == "secret"
            || secret.to_ascii_lowercase() == "changeme"
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
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("trusted_proxy_cidrs", &self.trusted_proxy_cidrs)
            .finish()
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
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
            database_url: "sqlite:test.db".into(),
            network: "testnet".into(),
            horizon_url: "https://horizon-testnet.stellar.org".into(),
            gateway_public: "GPUBLIC".into(),
            accepted_assets: AcceptedAsset::default_list(),
            webhook_secret: "webhook-hmac-secret".into(),
            webhook_retry_attempts: 3,
            webhook_retry_delay_ms: 5000,
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
            poll_max_pages_per_cycle: 50,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10,
            db_pool_max_connections: 10,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            webhook_allow_private_targets: false,
            admin_provisioning_secret: "admin-super-secret".into(),
            request_timeout_secs: 30,
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

    fn sample_config() -> Config {
        Config {
            port: 3000,
            database_url: "sqlite::memory:".into(),
            network: "testnet".into(),
            horizon_url: "https://horizon-testnet.stellar.org".into(),
            gateway_public: "UNCONFIGURED".into(),
            accepted_assets: AcceptedAsset::default_list(),
            webhook_secret: String::new(),
            webhook_retry_attempts: 3,
            webhook_retry_delay_ms: 5000,
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
            poll_max_pages_per_cycle: 50,
            payment_ttl_secs: 3600,
            rate_limit_requests_per_sec: 10,
            db_pool_max_connections: 10,
            db_busy_timeout_ms: 5000,
            cors_allowed_origins: vec![],
            listener_mode: ListenerMode::Stream,
            webhook_allow_private_targets: false,
            admin_provisioning_secret: String::new(),
            request_timeout_secs: 30,
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
        let res = Config::validate_webhook_secret(Ok(secret.into())).unwrap();
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
            .unwrap();

        // Backup current env values
        let backups: Vec<(String, Option<String>)> = env_vars
            .iter()
            .map(|(key, _)| (key.to_string(), std::env::var(key).ok()))
            .collect();

        // Set new values
        for &(key, val) in env_vars {
            if let Some(v) = val {
                std::env::set_var(key, v);
            } else {
                std::env::remove_var(key);
            }
        }

        // Run the test logic
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        // Restore backups
        for (key, val) in backups {
            if let Some(v) = val {
                std::env::set_var(key, v);
            } else {
                std::env::remove_var(key);
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
                let cfg = Config::from_env().unwrap();
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

    #[test]
    fn timing_rejects_zero_poll_interval() {
        let mut cfg = timing_config();
        cfg.poll_interval_secs = 0;
        let err = cfg.validate_timing().unwrap_err().to_string();
        assert!(err.contains("POLL_INTERVAL_SECS"), "got: {err}");
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
        assert_eq!(ListenerMode::parse("").unwrap(), ListenerMode::Stream);
    }

    #[test]
    fn listener_mode_stream_parses() {
        assert_eq!(ListenerMode::parse("stream").unwrap(), ListenerMode::Stream);
        assert_eq!(ListenerMode::parse("STREAM").unwrap(), ListenerMode::Stream);
    }

    #[test]
    fn listener_mode_poll_parses() {
        assert_eq!(ListenerMode::parse("poll").unwrap(), ListenerMode::Poll);
        assert_eq!(ListenerMode::parse("POLL").unwrap(), ListenerMode::Poll);
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

    // ── CURSOR_STALENESS_MULTIPLE ────────────────────────────────────────────

    /// Every `run_with_env` closure below must set a valid WEBHOOK_SECRET (and
    /// anything else `from_env` hard-requires), otherwise the panic inside the
    /// closure poisons the shared env-test mutex and every subsequent
    /// `run_with_env` test fails at the lock.
    const ENV_WEBHOOK_SECRET: &str = "a-very-long-and-secure-webhook-signing-secret-32-chars";

    #[test]
    fn cursor_staleness_multiple_defaults_to_three() {
        run_with_env(&[("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET))], || {
            assert_eq!(Config::from_env().unwrap().cursor_staleness_multiple, 3);
        });
    }

    #[test]
    fn cursor_staleness_multiple_parses_from_env() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("CURSOR_STALENESS_MULTIPLE", Some("7")),
            ],
            || {
                assert_eq!(Config::from_env().unwrap().cursor_staleness_multiple, 7);
            },
        );
    }

    #[test]
    fn cursor_staleness_multiple_rejects_non_numeric_value() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("CURSOR_STALENESS_MULTIPLE", Some("soon")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(
                    err.contains("CURSOR_STALENESS_MULTIPLE"),
                    "boot should abort on a non-numeric value; got: {err}"
                );
            },
        );
    }

    // ── WebhookPayloadDetail::parse (issue #306) ─────────────────────────────

    #[test]
    fn webhook_payload_detail_empty_defaults_to_minimal() {
        assert_eq!(
            WebhookPayloadDetail::parse("").unwrap(),
            WebhookPayloadDetail::Minimal
        );
    }

    #[test]
    fn webhook_payload_detail_minimal_parses() {
        assert_eq!(
            WebhookPayloadDetail::parse("minimal").unwrap(),
            WebhookPayloadDetail::Minimal
        );
        assert_eq!(
            WebhookPayloadDetail::parse("MINIMAL").unwrap(),
            WebhookPayloadDetail::Minimal
        );
    }

    #[test]
    fn webhook_payload_detail_full_parses() {
        assert_eq!(
            WebhookPayloadDetail::parse("full").unwrap(),
            WebhookPayloadDetail::Full
        );
        assert_eq!(
            WebhookPayloadDetail::parse("FULL").unwrap(),
            WebhookPayloadDetail::Full
        );
    }

    #[test]
    fn webhook_payload_detail_invalid_aborts_boot() {
        let err = WebhookPayloadDetail::parse("rich").unwrap_err().to_string();
        assert!(
            err.contains("WEBHOOK_PAYLOAD_DETAIL"),
            "error should name the variable; got: {err}"
        );
        assert!(
            err.contains("rich"),
            "error should echo the bad value; got: {err}"
        );
    }

    #[test]
    fn from_env_defaults_to_minimal_webhook_payload_detail() {
        run_with_env(&[("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET))], || {
            let cfg = Config::from_env().unwrap();
            assert_eq!(cfg.webhook_payload_detail, WebhookPayloadDetail::Minimal);
        });
    }

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
                Config::from_env().unwrap();
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
                Config::from_env().unwrap();
                assert!(!logs_contain("ALLOWED_WEBHOOK_SCHEMES"));
            },
        );
    }

    // ── Deployment-tunable limits (issue #279) ────────────────────────────────
    //
    // MAX_BODY_BYTES, RATE_LIMITER_MAX_KEYS, RATE_LIMITER_IDLE_TTL_SECS,
    // PAGINATION_DEFAULT_LIMIT, PAGINATION_MAX_LIMIT, SHUTDOWN_GRACE_SECS,
    // HORIZON_PAGE_LIMIT, DB_PRUNE_BATCH_SIZE, RETENTION_MAX_ROWS_PER_CYCLE.

    #[test]
    fn limits_default_from_sample_config_pass() {
        assert!(sample_config().validate_limits().is_ok());
    }

    #[test]
    fn limits_rejects_zero_max_body_bytes() {
        let mut cfg = sample_config();
        cfg.max_body_bytes = 0;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("MAX_BODY_BYTES"), "got: {err}");
    }

    #[test]
    fn limits_rejects_zero_rate_limiter_max_keys() {
        let mut cfg = sample_config();
        cfg.rate_limiter_max_keys = 0;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("RATE_LIMITER_MAX_KEYS"), "got: {err}");
    }

    #[test]
    fn limits_rejects_zero_rate_limiter_idle_ttl() {
        let mut cfg = sample_config();
        cfg.rate_limiter_idle_ttl_secs = 0;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("RATE_LIMITER_IDLE_TTL_SECS"), "got: {err}");
    }

    #[test]
    fn limits_rejects_zero_pagination_default_limit() {
        let mut cfg = sample_config();
        cfg.pagination_default_limit = 0;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("PAGINATION_DEFAULT_LIMIT"), "got: {err}");
    }

    #[test]
    fn limits_rejects_pagination_max_below_default() {
        let mut cfg = sample_config();
        cfg.pagination_default_limit = 50;
        cfg.pagination_max_limit = 10;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("PAGINATION_MAX_LIMIT"), "got: {err}");
        assert!(err.contains("PAGINATION_DEFAULT_LIMIT"), "got: {err}");
    }

    #[test]
    fn limits_allows_pagination_max_equal_to_default() {
        let mut cfg = sample_config();
        cfg.pagination_default_limit = 20;
        cfg.pagination_max_limit = 20;
        assert!(cfg.validate_limits().is_ok());
    }

    #[test]
    fn limits_rejects_zero_shutdown_grace() {
        let mut cfg = sample_config();
        cfg.shutdown_grace_secs = 0;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("SHUTDOWN_GRACE_SECS"), "got: {err}");
    }

    #[test]
    fn limits_rejects_zero_horizon_page_limit() {
        let mut cfg = sample_config();
        cfg.horizon_page_limit = 0;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("HORIZON_PAGE_LIMIT"), "got: {err}");
    }

    #[test]
    fn limits_rejects_zero_db_prune_batch_size() {
        let mut cfg = sample_config();
        cfg.db_prune_batch_size = 0;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("DB_PRUNE_BATCH_SIZE"), "got: {err}");
    }

    #[test]
    fn limits_rejects_zero_retention_max_rows_per_cycle() {
        let mut cfg = sample_config();
        cfg.retention_max_rows_per_cycle = 0;
        let err = cfg.validate_limits().unwrap_err().to_string();
        assert!(err.contains("RETENTION_MAX_ROWS_PER_CYCLE"), "got: {err}");
    }

    #[test]
    fn limits_parse_from_env_and_override_defaults() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("MAX_BODY_BYTES", Some("1024")),
                ("RATE_LIMITER_MAX_KEYS", Some("500")),
                ("RATE_LIMITER_IDLE_TTL_SECS", Some("120")),
                ("PAGINATION_DEFAULT_LIMIT", Some("5")),
                ("PAGINATION_MAX_LIMIT", Some("50")),
                ("SHUTDOWN_GRACE_SECS", Some("45")),
                ("HORIZON_PAGE_LIMIT", Some("100")),
                ("DB_PRUNE_BATCH_SIZE", Some("250")),
                ("RETENTION_MAX_ROWS_PER_CYCLE", Some("1000")),
            ],
            || {
                let cfg = Config::from_env().unwrap();
                assert_eq!(cfg.max_body_bytes, 1024);
                assert_eq!(cfg.rate_limiter_max_keys, 500);
                assert_eq!(cfg.rate_limiter_idle_ttl_secs, 120);
                assert_eq!(cfg.pagination_default_limit, 5);
                assert_eq!(cfg.pagination_max_limit, 50);
                assert_eq!(cfg.shutdown_grace_secs, 45);
                assert_eq!(cfg.horizon_page_limit, 100);
                assert_eq!(cfg.db_prune_batch_size, 250);
                assert_eq!(cfg.retention_max_rows_per_cycle, 1000);
            },
        );
    }

    #[test]
    fn limits_default_to_the_previous_compile_time_constants() {
        run_with_env(&[("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET))], || {
            let cfg = Config::from_env().unwrap();
            assert_eq!(cfg.max_body_bytes, 256 * 1024);
            assert_eq!(cfg.rate_limiter_max_keys, 10_000);
            assert_eq!(cfg.rate_limiter_idle_ttl_secs, 60);
            assert_eq!(cfg.pagination_default_limit, 20);
            assert_eq!(cfg.pagination_max_limit, 100);
            assert_eq!(cfg.shutdown_grace_secs, 30);
            assert_eq!(cfg.horizon_page_limit, 200);
            assert_eq!(cfg.db_prune_batch_size, 500);
            assert_eq!(cfg.retention_max_rows_per_cycle, 50_000);
        });
    }

    #[test]
    fn limits_invalid_env_value_aborts_boot() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("HORIZON_PAGE_LIMIT", Some("not-a-number")),
            ],
            || {
                let err = Config::from_env().unwrap_err().to_string();
                assert!(err.contains("HORIZON_PAGE_LIMIT"), "got: {err}");
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
                let cfg = Config::from_env().unwrap();
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
        assert!(result.is_ok(), "absent secret should succeed; got: {:?}", result);
        assert_eq!(result.unwrap(), "");
    }

    /// An explicitly empty env var is the same as absent.
    #[test]
    fn admin_secret_explicit_empty_string_disables_provisioning() {
        let result = Config::validate_admin_secret(Ok(String::new()));
        assert!(result.is_ok(), "empty string should succeed; got: {:?}", result);
        assert_eq!(result.unwrap(), "");
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
        assert_eq!(result.unwrap(), strong);
    }

    /// The whole from_env path must accept a strong admin secret.
    #[test]
    fn admin_secret_from_env_strong_value_is_accepted() {
        run_with_env(
            &[
                ("WEBHOOK_SECRET", Some(ENV_WEBHOOK_SECRET)),
                ("ADMIN_PROVISIONING_SECRET", Some(
                    "a3f8b2c1d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1",
                )),
            ],
            || {
                let cfg = Config::from_env().unwrap();
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
}
