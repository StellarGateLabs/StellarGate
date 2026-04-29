use anyhow::Result;

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub database_url: String,
    pub horizon_url: String,
    pub gateway_public: String,
    pub usdc_issuer: String,
    pub webhook_secret: String,
    pub webhook_retry_attempts: u32,
    pub webhook_retry_delay_ms: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            port: std::env::var("PORT")
                .unwrap_or_else(|_| "3000".into())
                .parse()
                .unwrap_or(3000),
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:stellargate.db".into()),
            horizon_url: std::env::var("STELLAR_HORIZON_URL")
                .unwrap_or_else(|_| "https://horizon-testnet.stellar.org".into()),
            gateway_public: std::env::var("STELLAR_GATEWAY_PUBLIC")
                .unwrap_or_else(|_| "UNCONFIGURED".into()),
            usdc_issuer: std::env::var("USDC_ISSUER")
                .unwrap_or_else(|_| "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".into()),
            webhook_secret: std::env::var("WEBHOOK_SECRET")
                .unwrap_or_else(|_| "default-secret".into()),
            webhook_retry_attempts: std::env::var("WEBHOOK_RETRY_ATTEMPTS")
                .unwrap_or_else(|_| "3".into())
                .parse()
                .unwrap_or(3),
            webhook_retry_delay_ms: std::env::var("WEBHOOK_RETRY_DELAY_MS")
                .unwrap_or_else(|_| "5000".into())
                .parse()
                .unwrap_or(5000),
        })
    }
}
