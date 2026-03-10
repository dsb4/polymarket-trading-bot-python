/// Backtesting engine for the Polymarket arbitrage strategy.
///
/// Approach:
/// 1. Fetch historical orderbook / price data from Polymarket's Data API.
/// 2. For each time-step, check if YES + NO ask < threshold (same logic as live bot).
/// 3. Simulate execution with configurable fee rate and slippage model.
/// 4. Compute performance metrics: total return, Sharpe ratio, max drawdown, win rate.
///
/// Data source:
/// - `GET /markets/{condition_id}/history` from data-api.polymarket.com
///   Returns price history as an array of { t: unix_ts, p: price } tuples.
///   We use this to reconstruct YES/NO price series and detect arb windows.
use crate::{
    config::Config,
    error::{BotError, Result},
    models::{BacktestResult, BacktestTrade, HistoricalPrice},
};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tracing::{info, warn};

/// Fee rate applied to each leg (Polymarket is typically ~1% combined).
const DEFAULT_FEE_RATE: Decimal = dec!(0.01);

/// Simulated slippage: orders fill at slightly worse prices than displayed.
const SLIPPAGE_FACTOR: Decimal = dec!(0.002); // 0.2% adverse slippage

pub struct BacktestEngine {
    client: Client,
    config: Config,
    fee_rate: Decimal,
}

// ── Historical price point from data API ──────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct PriceHistoryPoint {
    #[serde(rename = "t")]
    timestamp: i64,
    #[serde(rename = "p")]
    price: String,
}

#[derive(Debug, serde::Deserialize)]
struct MarketHistoryResponse {
    history: Vec<PriceHistoryPoint>,
}

// ── Backtest market definition ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BacktestMarket {
    pub condition_id: String,
    pub question: String,
    pub yes_token_id: String,
    pub no_token_id: String,
}

// ── Engine implementation ─────────────────────────────────────────────────────

impl BacktestEngine {
    pub fn new(config: Config) -> Result<Self> {
        let mut builder = Client::builder()
            .timeout(std::time::Duration::from_secs(30));

        if let Some(proxy_url) = config.get_socks5_proxy_url() {
            builder = builder
                .proxy(reqwest::Proxy::all(&proxy_url).map_err(|e| BotError::Api(e.to_string()))?);
        }

        Ok(Self {
            client: builder.build()?,
            config,
            fee_rate: DEFAULT_FEE_RATE,
        })
    }

    /// Run a full backtest over the given markets and date range.
    pub async fn run(
        &self,
        markets: Vec<BacktestMarket>,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
        initial_capital: Decimal,
        trade_size: Decimal,
    ) -> Result<BacktestResult> {
        info!(
            "Backtest: {} markets, {} → {}, capital={}",
            markets.len(),
            start_date.format("%Y-%m-%d"),
            end_date.format("%Y-%m-%d"),
            initial_capital,
        );

        let threshold = Decimal::try_from(self.config.min_profit_threshold).unwrap_or(dec!(0.005));

        let mut all_trades: Vec<BacktestTrade> = Vec::new();
        let mut capital = initial_capital;

        // Fetch price history for each market
        for market in &markets {
            let trades = self
                .backtest_market(market, start_date, end_date, threshold, trade_size, &mut capital)
                .await;

            match trades {
                Ok(t) => {
                    info!(
                        "  {} → {} trades",
                        &market.question[..market.question.len().min(50)],
                        t.len()
                    );
                    all_trades.extend(t);
                }
                Err(e) => {
                    warn!("  Skipping {}: {}", market.question, e);
                }
            }
        }

        // Sort by timestamp
        all_trades.sort_by_key(|t| t.timestamp);

        Ok(self.compute_metrics(all_trades, initial_capital, capital, start_date, end_date))
    }

    async fn backtest_market(
        &self,
        market: &BacktestMarket,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
        threshold: Decimal,
        trade_size: Decimal,
        capital: &mut Decimal,
    ) -> Result<Vec<BacktestTrade>> {
        // Fetch YES and NO price histories concurrently
        let (yes_history, no_history) = tokio::try_join!(
            self.fetch_price_history(&market.yes_token_id, start_date, end_date),
            self.fetch_price_history(&market.no_token_id, start_date, end_date),
        )?;

        if yes_history.is_empty() || no_history.is_empty() {
            return Ok(Vec::new());
        }

        // Align price histories by timestamp (interpolate/join)
        let aligned = align_price_series(&yes_history, &no_history);

        let mut trades = Vec::new();
        // Track open window to avoid double-counting
        let mut in_trade = false;
        let mut last_trade_ts: Option<DateTime<Utc>> = None;

        for (ts, yes_ask, no_ask) in aligned {
            let combined = yes_ask + no_ask;
            let profit = Decimal::ONE - combined;

            if profit <= threshold {
                in_trade = false;
                continue;
            }

            // Avoid entering same market window multiple times within 5 minutes
            if let Some(last) = last_trade_ts {
                if ts - last < Duration::minutes(5) {
                    continue;
                }
            }

            // Check capital
            let required = trade_size * combined;
            if *capital < required {
                continue;
            }

            // Apply slippage: prices are slightly worse in simulation
            let yes_fill = yes_ask * (Decimal::ONE + SLIPPAGE_FACTOR);
            let no_fill = no_ask * (Decimal::ONE + SLIPPAGE_FACTOR);
            let fill_combined = yes_fill + no_fill;
            let gross_profit = (Decimal::ONE - fill_combined) * trade_size;
            let fee = fill_combined * trade_size * self.fee_rate;
            let net_profit = gross_profit - fee;

            let filled = net_profit > Decimal::ZERO;

            let trade = BacktestTrade {
                timestamp: ts,
                market: market.question[..market.question.len().min(60)].to_string(),
                yes_price: yes_ask,
                no_price: no_ask,
                combined,
                profit_pct: profit,
                trade_size,
                gross_profit,
                fee,
                net_profit,
                filled,
            };

            if filled {
                *capital = *capital - required + trade_size + net_profit;
            }

            in_trade = true;
            last_trade_ts = Some(ts);
            trades.push(trade);
        }

        Ok(trades)
    }

    async fn fetch_price_history(
        &self,
        token_id: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<HistoricalPrice>> {
        let url = format!(
            "{}/prices-history",
            self.config.data_api_base_url
        );

        let resp = self
            .client
            .get(&url)
            .query(&[
                ("market", token_id),
                ("startTs", &start.timestamp().to_string()),
                ("endTs", &end.timestamp().to_string()),
                ("interval", "1m"),  // 1-minute candles
                ("fidelity", "60"),
            ])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BotError::Http { status, body });
        }

        let raw: MarketHistoryResponse = resp.json().await?;
        let prices: Vec<HistoricalPrice> = raw
            .history
            .into_iter()
            .filter_map(|p| {
                let ts = DateTime::from_timestamp(p.timestamp, 0)?;
                let price = p.price.parse::<Decimal>().ok()?;
                Some(HistoricalPrice {
                    timestamp: ts,
                    yes_price: price,  // Re-used field — caller knows which token
                    no_price: Decimal::ZERO,
                    volume: Decimal::ZERO,
                })
            })
            .collect();

        Ok(prices)
    }

    fn compute_metrics(
        &self,
        trades: Vec<BacktestTrade>,
        initial_capital: Decimal,
        final_capital: Decimal,
        start_date: DateTime<Utc>,
        end_date: DateTime<Utc>,
    ) -> BacktestResult {
        if trades.is_empty() {
            return BacktestResult {
                start_date,
                end_date,
                initial_capital,
                final_capital,
                total_return_pct: Decimal::ZERO,
                total_trades: 0,
                winning_trades: 0,
                losing_trades: 0,
                win_rate: Decimal::ZERO,
                avg_profit_per_trade: Decimal::ZERO,
                total_gross_profit: Decimal::ZERO,
                total_fees: Decimal::ZERO,
                total_net_profit: Decimal::ZERO,
                max_drawdown_pct: Decimal::ZERO,
                sharpe_ratio: Decimal::ZERO,
                avg_profit_pct: Decimal::ZERO,
                best_trade_profit: Decimal::ZERO,
                worst_trade_loss: Decimal::ZERO,
                trades: Vec::new(),
            };
        }

        let total_trades = trades.len() as u64;
        let winning = trades.iter().filter(|t| t.net_profit > Decimal::ZERO).count() as u64;
        let losing = total_trades - winning;
        let win_rate = if total_trades > 0 {
            Decimal::from(winning) / Decimal::from(total_trades)
        } else {
            Decimal::ZERO
        };

        let total_gross: Decimal = trades.iter().map(|t| t.gross_profit).sum();
        let total_fees: Decimal = trades.iter().map(|t| t.fee).sum();
        let total_net: Decimal = trades.iter().map(|t| t.net_profit).sum();
        let avg_profit = if total_trades > 0 {
            total_net / Decimal::from(total_trades)
        } else {
            Decimal::ZERO
        };
        let avg_profit_pct: Decimal = trades.iter().map(|t| t.profit_pct).sum::<Decimal>()
            / Decimal::from(total_trades);

        let best = trades.iter().map(|t| t.net_profit).max().unwrap_or(Decimal::ZERO);
        let worst = trades.iter().map(|t| t.net_profit).min().unwrap_or(Decimal::ZERO);

        let total_return_pct = if initial_capital.is_zero() {
            Decimal::ZERO
        } else {
            (final_capital - initial_capital) / initial_capital * Decimal::from(100)
        };

        // Max drawdown calculation
        let max_drawdown_pct = compute_max_drawdown(&trades, initial_capital);

        // Sharpe ratio (annualized, assuming 252 trading days, risk-free rate = 0)
        let sharpe = compute_sharpe_ratio(&trades);

        BacktestResult {
            start_date,
            end_date,
            initial_capital,
            final_capital,
            total_return_pct,
            total_trades,
            winning_trades: winning,
            losing_trades: losing,
            win_rate,
            avg_profit_per_trade: avg_profit,
            total_gross_profit: total_gross,
            total_fees,
            total_net_profit: total_net,
            max_drawdown_pct,
            sharpe_ratio: sharpe,
            avg_profit_pct,
            best_trade_profit: best,
            worst_trade_loss: worst,
            trades,
        }
    }
}

// ── Statistical helpers ───────────────────────────────────────────────────────

/// Align two price series (YES and NO) by timestamp, returning tuples of
/// (ts, yes_price, no_price). We forward-fill missing prices.
fn align_price_series(
    yes: &[HistoricalPrice],
    no: &[HistoricalPrice],
) -> Vec<(DateTime<Utc>, Decimal, Decimal)> {
    // Build a map of timestamp → price for NO
    let no_map: HashMap<i64, Decimal> = no
        .iter()
        .map(|p| (p.timestamp.timestamp(), p.yes_price))
        .collect();

    let mut result = Vec::new();
    let mut last_no: Option<Decimal> = None;

    for point in yes {
        let ts_secs = point.timestamp.timestamp();
        let no_price = no_map.get(&ts_secs).copied().or(last_no);
        if let Some(no_p) = no_price {
            last_no = Some(no_p);
            result.push((point.timestamp, point.yes_price, no_p));
        }
    }

    result
}

/// Compute max drawdown as a percentage of peak equity.
fn compute_max_drawdown(trades: &[BacktestTrade], initial_capital: Decimal) -> Decimal {
    let mut equity = initial_capital;
    let mut peak = initial_capital;
    let mut max_dd = Decimal::ZERO;

    for trade in trades {
        equity += trade.net_profit;
        if equity > peak {
            peak = equity;
        }
        if peak > Decimal::ZERO {
            let dd = (peak - equity) / peak * Decimal::from(100);
            if dd > max_dd {
                max_dd = dd;
            }
        }
    }

    max_dd
}

/// Compute annualized Sharpe ratio from trade returns.
/// Assumes ~252 trading days, risk-free rate = 0.
fn compute_sharpe_ratio(trades: &[BacktestTrade]) -> Decimal {
    if trades.len() < 2 {
        return Decimal::ZERO;
    }

    let returns: Vec<f64> = trades
        .iter()
        .map(|t| f64::try_from(t.profit_pct).unwrap_or(0.0))
        .collect();

    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std_dev = variance.sqrt();

    if std_dev < 1e-10 {
        return Decimal::ZERO;
    }

    // Annualize: assume ~252*24*60 minutes of trading per year, scale by sqrt(N_annual)
    // For per-trade Sharpe, use sqrt(252) annualization factor
    let sharpe = mean / std_dev * (252f64).sqrt();
    Decimal::try_from(sharpe).unwrap_or(Decimal::ZERO)
}

// ── Convenience: run backtest from CLI ───────────────────────────────────────

/// Run a demo backtest using synthetic price data (for testing without API access).
pub async fn run_demo_backtest(config: Config) -> Result<BacktestResult> {
    let engine = BacktestEngine::new(config)?;

    let initial_capital = dec!(10_000);
    let trade_size = dec!(100);
    let end = Utc::now();
    let start = end - Duration::days(30);

    // Generate synthetic arbitrage opportunities
    let trades = generate_synthetic_trades(start, end, trade_size, dec!(0.005));

    let total_net: Decimal = trades.iter().map(|t| t.net_profit).sum();
    let final_capital = initial_capital + total_net;

    Ok(engine.compute_metrics(trades, initial_capital, final_capital, start, end))
}

/// Generate synthetic trade history for demo purposes.
fn generate_synthetic_trades(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    trade_size: Decimal,
    threshold: Decimal,
) -> Vec<BacktestTrade> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut trades = Vec::new();
    let mut current = start;
    let markets = [
        "BTC above $50k by EOD?",
        "ETH above $3k by Friday?",
        "Fed raises rates in March?",
        "Trump wins 2024 election?",
        "SpaceX launches by June?",
    ];

    while current < end {
        // Random time advance: 1-8 hours
        let hours = rng.gen_range(1..8);
        current = current + Duration::hours(hours);

        // Generate a random opportunity
        let yes_ask = Decimal::try_from(rng.gen_range(0.35_f64..0.48_f64)).unwrap();
        let no_ask = Decimal::try_from(rng.gen_range(0.35_f64..0.48_f64)).unwrap();
        let combined = yes_ask + no_ask;
        let profit = Decimal::ONE - combined;

        if profit < threshold {
            continue;
        }

        let yes_fill = yes_ask * (Decimal::ONE + SLIPPAGE_FACTOR);
        let no_fill = no_ask * (Decimal::ONE + SLIPPAGE_FACTOR);
        let fill_combined = yes_fill + no_fill;
        let gross_profit = (Decimal::ONE - fill_combined) * trade_size;
        let fee = fill_combined * trade_size * DEFAULT_FEE_RATE;
        let net_profit = gross_profit - fee;

        trades.push(BacktestTrade {
            timestamp: current,
            market: markets[rng.gen_range(0..markets.len())].to_string(),
            yes_price: yes_ask,
            no_price: no_ask,
            combined,
            profit_pct: profit,
            trade_size,
            gross_profit,
            fee,
            net_profit,
            filled: net_profit > Decimal::ZERO,
        });
    }

    trades
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_max_drawdown_flat() {
        // All winning trades → drawdown = 0
        let trades: Vec<BacktestTrade> = (0..5)
            .map(|i| BacktestTrade {
                timestamp: Utc::now(),
                market: "test".into(),
                yes_price: dec!(0.48),
                no_price: dec!(0.49),
                combined: dec!(0.97),
                profit_pct: dec!(0.03),
                trade_size: dec!(100),
                gross_profit: dec!(3),
                fee: dec!(0.97),
                net_profit: dec!(2.03),
                filled: true,
            })
            .collect();
        let dd = compute_max_drawdown(&trades, dec!(1000));
        assert_eq!(dd, dec!(0));
    }

    #[test]
    fn test_max_drawdown_with_losses() {
        let make_trade = |profit: f64| BacktestTrade {
            timestamp: Utc::now(),
            market: "test".into(),
            yes_price: dec!(0.48),
            no_price: dec!(0.49),
            combined: dec!(0.97),
            profit_pct: dec!(0.03),
            trade_size: dec!(100),
            gross_profit: Decimal::try_from(profit + 0.97).unwrap(),
            fee: Decimal::try_from(0.97_f64).unwrap(),
            net_profit: Decimal::try_from(profit).unwrap(),
            filled: profit > 0.0,
        };

        let trades = vec![
            make_trade(10.0),  // equity 1010
            make_trade(-50.0), // equity 960 → drawdown = 50/1010 = 4.95%
            make_trade(20.0),  // equity 980
        ];
        let dd = compute_max_drawdown(&trades, dec!(1000));
        // Peak was 1010, trough was 960 → dd = 50/1010 ≈ 4.95%
        assert!(dd > dec!(4) && dd < dec!(6), "drawdown={}", dd);
    }

    #[test]
    fn test_align_price_series_basic() {
        let yes = vec![
            HistoricalPrice {
                timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
                yes_price: dec!(0.48),
                no_price: Decimal::ZERO,
                volume: Decimal::ZERO,
            },
            HistoricalPrice {
                timestamp: DateTime::from_timestamp(1060, 0).unwrap(),
                yes_price: dec!(0.52),
                no_price: Decimal::ZERO,
                volume: Decimal::ZERO,
            },
        ];
        let no = vec![HistoricalPrice {
            timestamp: DateTime::from_timestamp(1000, 0).unwrap(),
            yes_price: dec!(0.49),
            no_price: Decimal::ZERO,
            volume: Decimal::ZERO,
        }];

        let aligned = align_price_series(&yes, &no);
        // First point aligns, second uses forward-filled no price
        assert_eq!(aligned.len(), 2);
        assert_eq!(aligned[0].1, dec!(0.48)); // yes
        assert_eq!(aligned[0].2, dec!(0.49)); // no
        assert_eq!(aligned[1].1, dec!(0.52)); // yes
        assert_eq!(aligned[1].2, dec!(0.49)); // forward-filled no
    }

    #[test]
    fn test_sharpe_ratio_uniform_returns() {
        // All same return → infinite Sharpe (returns 0 by our guard)
        let trades: Vec<BacktestTrade> = (0..10)
            .map(|_| BacktestTrade {
                timestamp: Utc::now(),
                market: "t".into(),
                yes_price: dec!(0.48),
                no_price: dec!(0.49),
                combined: dec!(0.97),
                profit_pct: dec!(0.03),
                trade_size: dec!(100),
                gross_profit: dec!(3),
                fee: dec!(0.97),
                net_profit: dec!(2.03),
                filled: true,
            })
            .collect();
        // std_dev ≈ 0 → returns 0 (guard)
        let sharpe = compute_sharpe_ratio(&trades);
        assert_eq!(sharpe, dec!(0));
    }

    #[tokio::test]
    async fn test_demo_backtest_runs() {
        dotenvy::dotenv().ok();
        let config = crate::config::Config::from_env()
            .unwrap_or_else(|_| make_test_config());
        let result = run_demo_backtest(config).await.unwrap();
        assert!(result.total_trades > 0);
        result.print_summary();
    }

    fn make_test_config() -> crate::config::Config {
        crate::config::Config {
            private_key: None,
            wallet_address: None,
            polygon_rpc_url: "".into(),
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
            volatility_skip_1min_std: None,
            min_seconds_until_resolution: 90,
            min_volume_60s_usd: None,
            max_zscore_3min: None,
            max_rsi_overbought: None,
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
}
