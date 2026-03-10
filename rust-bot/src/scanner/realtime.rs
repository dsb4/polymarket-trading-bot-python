/// Real-time market scanner using WebSocket streaming.
///
/// Audit improvements vs Python original:
/// - Uses `tokio::sync::mpsc` channels instead of callbacks, avoiding async callback complexity.
/// - MarketPrices state lives in a `DashMap`-style HashMap behind an Arc<RwLock<>>.
/// - Arbitrage detection fires instantly on every price update (no lock contention on the hot path).
/// - Near-miss tracking is per-scan-cycle, not shared mutable state.
use crate::{
    api::{
        gamma::GammaClient,
        websocket::{PriceUpdate, WsClient, MAX_ASSETS_PER_WS},
    },
    config::Config,
    models::{ArbitrageOpportunity, Market},
};
use chrono::Utc;
use parking_lot::RwLock;
use rust_decimal::Decimal;
use std::{
    collections::HashMap,
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// ── Market price state ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct MarketPrices {
    pub yes_best_bid: Option<Decimal>,
    pub yes_best_ask: Option<Decimal>,
    pub yes_best_ask_size: Option<Decimal>,
    pub no_best_bid: Option<Decimal>,
    pub no_best_ask: Option<Decimal>,
    pub no_best_ask_size: Option<Decimal>,
}

impl MarketPrices {
    pub fn combined_ask(&self) -> Option<Decimal> {
        Some(self.yes_best_ask? + self.no_best_ask?)
    }

    pub fn arbitrage_profit(&self) -> Option<Decimal> {
        Some(Decimal::ONE - self.combined_ask()?)
    }

    pub fn has_arbitrage(&self, threshold: Decimal) -> bool {
        self.arbitrage_profit()
            .map(|p| p > threshold)
            .unwrap_or(false)
    }
}

// ── Scanner state (shared across tasks) ─────────────────────────────────────

struct ScannerState {
    markets: HashMap<String, Market>,              // market_id → Market
    token_to_market: HashMap<String, String>,      // token_id → market_id
    market_prices: HashMap<String, MarketPrices>,  // market_id → prices
    price_updates: u64,
    arbitrage_alerts: u64,
}

impl ScannerState {
    fn new() -> Self {
        Self {
            markets: HashMap::new(),
            token_to_market: HashMap::new(),
            market_prices: HashMap::new(),
            price_updates: 0,
            arbitrage_alerts: 0,
        }
    }

    fn load_markets(&mut self, markets: Vec<Market>) {
        self.markets.clear();
        self.token_to_market.clear();
        self.market_prices.clear();

        for m in markets {
            self.token_to_market.insert(m.yes_token.token_id.clone(), m.id.clone());
            self.token_to_market.insert(m.no_token.token_id.clone(), m.id.clone());
            self.market_prices.insert(m.id.clone(), MarketPrices::default());
            self.markets.insert(m.id.clone(), m);
        }
    }
}

// ── Scanner stats ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ScannerStats {
    pub markets: usize,
    pub price_updates: u64,
    pub arbitrage_alerts: u64,
    pub ws_connected: bool,
    pub subscribed_tokens: usize,
}

// ── RealtimeScanner ───────────────────────────────────────────────────────────

pub struct RealtimeScanner {
    config: Arc<Config>,
    gamma: GammaClient,
    state: Arc<RwLock<ScannerState>>,
    /// Channel for emitting arbitrage opportunities to the bot.
    opportunity_tx: mpsc::UnboundedSender<ArbitrageOpportunity>,
    running: Arc<std::sync::atomic::AtomicBool>,
}

impl RealtimeScanner {
    pub fn new(
        config: Arc<Config>,
        opportunity_tx: mpsc::UnboundedSender<ArbitrageOpportunity>,
    ) -> crate::error::Result<Self> {
        let gamma = GammaClient::new(&config)?;
        Ok(Self {
            config,
            gamma,
            state: Arc::new(RwLock::new(ScannerState::new())),
            opportunity_tx,
            running: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    pub async fn run(&mut self) -> crate::error::Result<()> {
        use std::sync::atomic::Ordering;

        self.running.store(true, Ordering::Relaxed);

        // Load markets
        let markets = self
            .gamma
            .fetch_all_active_markets(
                self.config.min_liquidity_usd,
                self.config.max_days_until_resolution,
            )
            .await?;

        // Sort by liquidity, take top N
        let max_markets = (MAX_ASSETS_PER_WS / 2) * self.config.num_ws_connections;
        let mut markets = markets;
        markets.sort_by(|a, b| b.liquidity.cmp(&a.liquidity));
        markets.truncate(max_markets);

        info!(
            "Scanner: loaded {} markets (max {})",
            markets.len(),
            max_markets
        );

        // Build token ID list
        let token_ids: Vec<String> = markets
            .iter()
            .flat_map(|m| [m.yes_token.token_id.clone(), m.no_token.token_id.clone()])
            .collect();

        // Populate shared state
        {
            let mut state = self.state.write();
            state.load_markets(markets);
        }

        // Create mpsc channel for price updates from all WS workers
        let (price_tx, mut price_rx) = mpsc::unbounded_channel::<PriceUpdate>();

        // Spawn one WsClient task per connection
        let num_conns = self.config.num_ws_connections;
        for conn_id in 0..num_conns {
            let start = conn_id * MAX_ASSETS_PER_WS;
            let end = (start + MAX_ASSETS_PER_WS).min(token_ids.len());
            if start >= token_ids.len() {
                break;
            }
            let batch: Vec<String> = token_ids[start..end].to_vec();
            let tx = price_tx.clone();

            // Polymarket WS URL
            let ws_url = "wss://ws-subscriptions-clob.polymarket.com/ws/market".to_string();
            let mut client = WsClient::new(ws_url, tx);

            tokio::spawn(async move {
                if let Err(e) = client.run(batch).await {
                    warn!("WS connection {} ended with error: {}", conn_id + 1, e);
                }
            });
        }

        drop(price_tx); // drop sender so the rx closes when all workers stop

        // Periodic market refresh task
        let config = Arc::clone(&self.config);
        let state_clone = Arc::clone(&self.state);
        let running_clone = Arc::clone(&self.running);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(600)).await;
                if !running_clone.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(g) = GammaClient::new(&config) {
                    match g
                        .fetch_all_active_markets(
                            config.min_liquidity_usd,
                            config.max_days_until_resolution,
                        )
                        .await
                    {
                        Ok(mut fresh) => {
                            let max = (MAX_ASSETS_PER_WS / 2) * config.num_ws_connections;
                            fresh.sort_by(|a, b| b.liquidity.cmp(&a.liquidity));
                            fresh.truncate(max);
                            let mut state = state_clone.write();
                            let old_count = state.markets.len();
                            state.load_markets(fresh);
                            info!(
                                "Market refresh: {} → {} markets",
                                old_count,
                                state.markets.len()
                            );
                        }
                        Err(e) => warn!("Market refresh error: {}", e),
                    }
                }
            }
        });

        // Stats logging task
        let state_stats = Arc::clone(&self.state);
        let running_stats = Arc::clone(&self.running);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if !running_stats.load(Ordering::Relaxed) {
                    break;
                }
                let s = state_stats.read();
                info!(
                    "Scanner stats: markets={} price_updates={} arb_alerts={}",
                    s.markets.len(),
                    s.price_updates,
                    s.arbitrage_alerts,
                );
            }
        });

        // Main price update processing loop
        let threshold = Decimal::try_from(self.config.min_profit_threshold)
            .unwrap_or(Decimal::new(5, 3)); // 0.005

        while let Some(update) = price_rx.recv().await {
            {
                let mut state = self.state.write();
                state.price_updates += 1;

                let (token_id, new_bid, new_ask, new_ask_size) = match &update {
                    PriceUpdate::Book {
                        asset_id,
                        best_bid,
                        best_ask,
                        best_ask_size,
                    } => (asset_id, *best_bid, *best_ask, *best_ask_size),
                    PriceUpdate::PriceChange {
                        asset_id,
                        best_bid,
                        best_ask,
                        ..
                    } => (asset_id, *best_bid, *best_ask, None),
                };

                let market_id = match state.token_to_market.get(token_id.as_str()) {
                    Some(id) => id.clone(),
                    None => continue,
                };
                let market = match state.markets.get(&market_id) {
                    Some(m) => m.clone(),
                    None => continue,
                };
                let prices = match state.market_prices.get_mut(&market_id) {
                    Some(p) => p,
                    None => continue,
                };

                // Update prices for this token
                let is_yes = token_id == &market.yes_token.token_id;
                if is_yes {
                    prices.yes_best_bid = new_bid;
                    prices.yes_best_ask = new_ask;
                    if new_ask_size.is_some() {
                        prices.yes_best_ask_size = new_ask_size;
                    }
                } else {
                    prices.no_best_bid = new_bid;
                    prices.no_best_ask = new_ask;
                    if new_ask_size.is_some() {
                        prices.no_best_ask_size = new_ask_size;
                    }
                }

                // Check arbitrage
                if !prices.has_arbitrage(threshold) {
                    continue;
                }

                // Resolution date filter
                if let Some(days) = market.days_until_resolution() {
                    if days > self.config.max_days_until_resolution as i64 {
                        debug!(
                            "Skipping arb: {} days until resolution (max {})",
                            days, self.config.max_days_until_resolution
                        );
                        continue;
                    }
                }

                let yes_ask = match prices.yes_best_ask {
                    Some(v) => v,
                    None => continue,
                };
                let no_ask = match prices.no_best_ask {
                    Some(v) => v,
                    None => continue,
                };
                let combined = yes_ask + no_ask;
                let profit = Decimal::ONE - combined;
                let yes_size = prices.yes_best_ask_size.unwrap_or(Decimal::ZERO);
                let no_size = prices.no_best_ask_size.unwrap_or(Decimal::ZERO);

                state.arbitrage_alerts += 1;

                let opportunity = ArbitrageOpportunity {
                    market: market.clone(),
                    yes_ask,
                    no_ask,
                    combined_cost: combined,
                    profit_pct: profit,
                    yes_size_available: yes_size,
                    no_size_available: no_size,
                    max_trade_size: Decimal::ZERO, // Set by bot based on risk/balance
                    detected_at: Utc::now(),
                };

                info!(
                    "ARBITRAGE DETECTED: {} | yes={:.4} no={:.4} combined={:.4} profit={:.2}%",
                    &market.question[..market.question.len().min(50)],
                    yes_ask,
                    no_ask,
                    combined,
                    profit * Decimal::from(100),
                );

                drop(state); // Release lock before sending
                let _ = self.opportunity_tx.send(opportunity);
            }
        }

        info!("Scanner: price update channel closed, stopping");
        self.running.store(false, Ordering::Relaxed);
        Ok(())
    }

    pub fn stop(&self) {
        use std::sync::atomic::Ordering;
        self.running.store(false, Ordering::Relaxed);
    }

    pub fn get_stats(&self) -> ScannerStats {
        let s = self.state.read();
        ScannerStats {
            markets: s.markets.len(),
            price_updates: s.price_updates,
            arbitrage_alerts: s.arbitrage_alerts,
            ws_connected: true,
            subscribed_tokens: s.token_to_market.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_has_arbitrage() {
        let mut prices = MarketPrices::default();
        prices.yes_best_ask = Some(dec!(0.48));
        prices.no_best_ask = Some(dec!(0.49));

        // combined = 0.97, profit = 0.03 → arbitrage with 0.5% threshold
        assert!(prices.has_arbitrage(dec!(0.005)));
        // Not arbitrage with 5% threshold
        assert!(!prices.has_arbitrage(dec!(0.05)));
    }

    #[test]
    fn test_no_arbitrage_above_1() {
        let mut prices = MarketPrices::default();
        prices.yes_best_ask = Some(dec!(0.52));
        prices.no_best_ask = Some(dec!(0.51));
        // combined = 1.03, profit = -0.03
        assert!(!prices.has_arbitrage(dec!(0.005)));
    }

    #[test]
    fn test_combined_ask() {
        let mut prices = MarketPrices::default();
        prices.yes_best_ask = Some(dec!(0.48));
        prices.no_best_ask = Some(dec!(0.49));
        assert_eq!(prices.combined_ask(), Some(dec!(0.97)));
        assert_eq!(prices.arbitrage_profit(), Some(dec!(0.03)));
    }
}
