/// Gamma API client for fetching active Polymarket markets.
use crate::{
    config::Config,
    error::{BotError, Result},
    models::{GammaMarket, Market},
};
use reqwest::Client;
use tracing::{debug, info, warn};

const PAGE_SIZE: usize = 100;

pub struct GammaClient {
    client: Client,
    base_url: String,
}

impl GammaClient {
    pub fn new(config: &Config) -> Result<Self> {
        let mut builder = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .pool_max_idle_per_host(10);

        if let Some(proxy_url) = config.get_socks5_proxy_url() {
            builder = builder
                .proxy(reqwest::Proxy::all(&proxy_url).map_err(|e| BotError::Api(e.to_string()))?);
        }

        Ok(Self {
            client: builder.build()?,
            base_url: config.gamma_base_url.clone(),
        })
    }

    /// Fetch all active markets, paginating until exhausted.
    pub async fn fetch_all_active_markets(
        &self,
        min_liquidity: f64,
        max_days_until_resolution: u32,
    ) -> Result<Vec<Market>> {
        let mut all_markets: Vec<Market> = Vec::new();
        let mut offset = 0usize;

        loop {
            let batch = self
                .fetch_markets_page(offset, PAGE_SIZE, min_liquidity)
                .await?;

            let count = batch.len();
            debug!("Fetched {} markets at offset {}", count, offset);

            for raw in batch {
                match Market::try_from(raw) {
                    Ok(m) => {
                        // Filter by resolution date
                        if let Some(days) = m.days_until_resolution() {
                            if days > max_days_until_resolution as i64 {
                                continue;
                            }
                        }
                        all_markets.push(m);
                    }
                    Err(e) => {
                        warn!("Skipping malformed market: {}", e);
                    }
                }
            }

            if count < PAGE_SIZE {
                break; // Last page
            }
            offset += PAGE_SIZE;
        }

        info!(
            "Gamma: fetched {} active markets (min_liquidity=${:.0}, max_days={})",
            all_markets.len(),
            min_liquidity,
            max_days_until_resolution
        );
        Ok(all_markets)
    }

    async fn fetch_markets_page(
        &self,
        offset: usize,
        limit: usize,
        min_liquidity: f64,
    ) -> Result<Vec<GammaMarket>> {
        let url = format!("{}/markets", self.base_url);
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("active", "true"),
                ("closed", "false"),
                ("limit", &limit.to_string()),
                ("offset", &offset.to_string()),
                ("liquidity_num_min", &min_liquidity.to_string()),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BotError::Http { status, body });
        }

        Ok(resp.json::<Vec<GammaMarket>>().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> Config {
        Config {
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
            socks5_proxy_host: None,
            socks5_proxy_port: 1080,
            socks5_proxy_user: None,
            socks5_proxy_pass: None,
        }
    }

    #[test]
    fn test_gamma_client_creates() {
        let cfg = test_config();
        let client = GammaClient::new(&cfg);
        assert!(client.is_ok());
    }
}
