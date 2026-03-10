use crate::error::{BotError, Result};
use std::env;

/// All configuration loaded from environment variables / .env file.
#[derive(Debug, Clone)]
pub struct Config {
    // Wallet
    pub private_key: Option<String>,
    pub wallet_address: Option<String>,

    // Network
    pub polygon_rpc_url: String,
    pub chain_id: u64,

    // Trading
    pub min_profit_threshold: f64,
    pub max_position_size: f64,
    pub poll_interval_seconds: f64,
    pub min_liquidity_usd: f64,
    pub max_days_until_resolution: u32,

    // Risk management
    pub risk_per_trade_pct: f64,
    pub stop_loss_pct: f64,
    pub time_stop_seconds: u32,
    pub position_cap_pct: f64,
    pub take_profit_pct_to_one: f64,
    pub take_profit_first_portion_pct: f64,
    pub consecutive_losses_pause: u32,
    pub consecutive_loss_pause_minutes: u32,
    pub session_drawdown_pct: f64,
    pub session_pause_minutes: u32,
    pub daily_drawdown_pct: f64,
    pub monthly_drawdown_pct: f64,
    pub volatility_skip_1min_std: Option<f64>,
    pub min_seconds_until_resolution: u32,
    pub min_volume_60s_usd: Option<f64>,
    pub max_zscore_3min: Option<f64>,
    pub max_rsi_overbought: Option<f64>,

    // WebSocket
    pub num_ws_connections: usize,

    // API endpoints
    pub clob_base_url: String,
    pub gamma_base_url: String,
    pub data_api_base_url: String,

    // Polymarket L2 API credentials
    pub poly_api_key: Option<String>,
    pub poly_api_secret: Option<String>,
    pub poly_api_passphrase: Option<String>,

    // Alerts
    pub slack_webhook_url: Option<String>,

    // Mode
    pub dry_run: bool,

    // Dashboard
    pub dashboard_username: String,
    pub dashboard_password: String,
    pub dashboard_port: u16,

    // Logging
    pub log_level: String,

    // SOCKS5 proxy
    pub socks5_proxy_host: Option<String>,
    pub socks5_proxy_port: u16,
    pub socks5_proxy_user: Option<String>,
    pub socks5_proxy_pass: Option<String>,
}

impl Config {
    /// Load configuration from environment variables (after dotenvy loads .env).
    pub fn from_env() -> Result<Self> {
        dotenvy::dotenv().ok();

        let private_key = opt_env("PRIVATE_KEY")?;
        let wallet_address = opt_env("WALLET_ADDRESS")?;

        // Validate wallet address format
        if let Some(ref addr) = wallet_address {
            if !addr.starts_with("0x") || addr.len() != 42 {
                return Err(BotError::Config(format!(
                    "WALLET_ADDRESS must be 0x + 40 hex chars, got: {}",
                    addr
                )));
            }
        }
        // Validate private key format
        if let Some(ref key) = private_key {
            if !key.starts_with("0x") || key.len() != 66 {
                return Err(BotError::Config(
                    "PRIVATE_KEY must be 0x + 64 hex chars".into(),
                ));
            }
        }

        Ok(Config {
            private_key,
            wallet_address: wallet_address.map(|a| a.to_lowercase()),

            polygon_rpc_url: env_or("POLYGON_RPC_URL", "https://polygon-rpc.com"),
            chain_id: env_parse("CHAIN_ID", 137)?,

            min_profit_threshold: env_parse("MIN_PROFIT_THRESHOLD", 0.005)?,
            max_position_size: env_parse("MAX_POSITION_SIZE", 100.0)?,
            poll_interval_seconds: env_parse("POLL_INTERVAL_SECONDS", 2.0)?,
            min_liquidity_usd: env_parse("MIN_LIQUIDITY_USD", 10_000.0)?,
            max_days_until_resolution: env_parse("MAX_DAYS_UNTIL_RESOLUTION", 7)?,

            risk_per_trade_pct: env_parse("RISK_PER_TRADE_PCT", 0.8)?,
            stop_loss_pct: env_parse("STOP_LOSS_PCT", 5.0)?,
            time_stop_seconds: env_parse("TIME_STOP_SECONDS", 120)?,
            position_cap_pct: env_parse("POSITION_CAP_PCT", 25.0)?,
            take_profit_pct_to_one: env_parse("TAKE_PROFIT_PCT_TO_ONE", 55.0)?,
            take_profit_first_portion_pct: env_parse("TAKE_PROFIT_FIRST_PORTION_PCT", 65.0)?,
            consecutive_losses_pause: env_parse("CONSECUTIVE_LOSSES_PAUSE", 5)?,
            consecutive_loss_pause_minutes: env_parse("CONSECUTIVE_LOSS_PAUSE_MINUTES", 30)?,
            session_drawdown_pct: env_parse("SESSION_DRAWDOWN_PCT", 4.0)?,
            session_pause_minutes: env_parse("SESSION_PAUSE_MINUTES", 60)?,
            daily_drawdown_pct: env_parse("DAILY_DRAWDOWN_PCT", 8.0)?,
            monthly_drawdown_pct: env_parse("MONTHLY_DRAWDOWN_PCT", 20.0)?,
            volatility_skip_1min_std: opt_env_parse("VOLATILITY_SKIP_1MIN_STD")?,
            min_seconds_until_resolution: env_parse("MIN_SECONDS_UNTIL_RESOLUTION", 90)?,
            min_volume_60s_usd: opt_env_parse("MIN_VOLUME_60S_USD")?,
            max_zscore_3min: opt_env_parse("MAX_ZSCORE_3MIN")?,
            max_rsi_overbought: opt_env_parse("MAX_RSI_OVERBOUGHT")?,

            num_ws_connections: env_parse("NUM_WS_CONNECTIONS", 6)?,

            clob_base_url: env_or("CLOB_BASE_URL", "https://clob.polymarket.com"),
            gamma_base_url: env_or("GAMMA_BASE_URL", "https://gamma-api.polymarket.com"),
            data_api_base_url: env_or("DATA_API_BASE_URL", "https://data-api.polymarket.com"),

            poly_api_key: opt_env("POLY_API_KEY")?,
            poly_api_secret: opt_env("POLY_API_SECRET")?,
            poly_api_passphrase: opt_env("POLY_API_PASSPHRASE")?,

            slack_webhook_url: opt_env("SLACK_WEBHOOK_URL")?,

            dry_run: env_bool("DRY_RUN", true),

            dashboard_username: env_or("DASHBOARD_USERNAME", "admin"),
            dashboard_password: env_or("DASHBOARD_PASSWORD", ""),
            dashboard_port: env_parse("DASHBOARD_PORT", 8080)?,

            log_level: env_or("LOG_LEVEL", "INFO"),

            socks5_proxy_host: opt_env("SOCKS5_PROXY_HOST")?,
            socks5_proxy_port: env_parse("SOCKS5_PROXY_PORT", 1080)?,
            socks5_proxy_user: opt_env("SOCKS5_PROXY_USER")?,
            socks5_proxy_pass: opt_env("SOCKS5_PROXY_PASS")?,
        })
    }

    pub fn is_trading_enabled(&self) -> bool {
        self.private_key.is_some() && self.wallet_address.is_some()
    }

    pub fn get_socks5_proxy_url(&self) -> Option<String> {
        let host = self.socks5_proxy_host.as_ref()?;
        match (&self.socks5_proxy_user, &self.socks5_proxy_pass) {
            (Some(user), Some(pass)) => Some(format!(
                "socks5h://{}:{}@{}:{}",
                user, pass, host, self.socks5_proxy_port
            )),
            _ => Some(format!("socks5h://{}:{}", host, self.socks5_proxy_port)),
        }
    }

    pub fn is_proxy_enabled(&self) -> bool {
        self.socks5_proxy_host.is_some()
    }
}

fn opt_env(key: &str) -> Result<Option<String>> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Ok(Some(v)),
        Ok(_) | Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(BotError::Config(format!("{}: {}", key, e))),
    }
}

fn opt_env_parse<T: std::str::FromStr>(key: &str) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(v) if !v.is_empty() => v
            .parse::<T>()
            .map(Some)
            .map_err(|e| BotError::Config(format!("{}: {}", key, e))),
        Ok(_) | Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(BotError::Config(format!("{}: {}", key, e))),
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match env::var(key) {
        Ok(v) => v
            .parse::<T>()
            .map_err(|e| BotError::Config(format!("{}: {}", key, e))),
        Err(_) => Ok(default),
    }
}

fn env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes"))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_socks5_url_with_auth() {
        let mut cfg = Config {
            private_key: None,
            wallet_address: None,
            polygon_rpc_url: "https://polygon-rpc.com".into(),
            chain_id: 137,
            min_profit_threshold: 0.005,
            max_position_size: 100.0,
            poll_interval_seconds: 2.0,
            min_liquidity_usd: 10000.0,
            max_days_until_resolution: 7,
            risk_per_trade_pct: 0.8,
            stop_loss_pct: 5.0,
            time_stop_seconds: 120,
            position_cap_pct: 25.0,
            take_profit_pct_to_one: 55.0,
            take_profit_first_portion_pct: 65.0,
            consecutive_losses_pause: 5,
            consecutive_loss_pause_minutes: 30,
            session_drawdown_pct: 4.0,
            session_pause_minutes: 60,
            daily_drawdown_pct: 8.0,
            monthly_drawdown_pct: 20.0,
            volatility_skip_1min_std: Some(0.028),
            min_seconds_until_resolution: 90,
            min_volume_60s_usd: None,
            max_zscore_3min: Some(2.5),
            max_rsi_overbought: Some(80.0),
            num_ws_connections: 6,
            clob_base_url: "https://clob.polymarket.com".into(),
            gamma_base_url: "https://gamma-api.polymarket.com".into(),
            data_api_base_url: "https://data-api.polymarket.com".into(),
            poly_api_key: None,
            poly_api_secret: None,
            poly_api_passphrase: None,
            slack_webhook_url: None,
            dry_run: true,
            dashboard_username: "admin".into(),
            dashboard_password: "".into(),
            dashboard_port: 8080,
            log_level: "INFO".into(),
            socks5_proxy_host: Some("proxy.example.com".into()),
            socks5_proxy_port: 1080,
            socks5_proxy_user: Some("user".into()),
            socks5_proxy_pass: Some("pass".into()),
        };

        let url = cfg.get_socks5_proxy_url();
        assert_eq!(url, Some("socks5h://user:pass@proxy.example.com:1080".into()));
        cfg.socks5_proxy_user = None;
        cfg.socks5_proxy_pass = None;
        let url = cfg.get_socks5_proxy_url();
        assert_eq!(url, Some("socks5h://proxy.example.com:1080".into()));
    }
}
