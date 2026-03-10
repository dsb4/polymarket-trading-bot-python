use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

// ── Market ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub token_id: String,
    pub outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Market {
    pub id: String,
    pub question: String,
    pub yes_token: Token,
    pub no_token: Token,
    pub volume: Decimal,
    pub liquidity: Decimal,
    pub end_date: Option<DateTime<Utc>>,
    pub active: bool,
    pub closed: bool,
    pub neg_risk: bool,
    pub condition_id: Option<String>,
}

impl Market {
    pub fn days_until_resolution(&self) -> Option<i64> {
        self.end_date.map(|end| {
            let now = Utc::now();
            (end - now).num_days()
        })
    }
}

// ── Order Book ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceLevel {
    pub price: Decimal,
    pub size: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBook {
    pub asset_id: String,
    pub bids: Vec<PriceLevel>,
    pub asks: Vec<PriceLevel>,
}

impl OrderBook {
    pub fn best_ask(&self) -> Option<Decimal> {
        self.asks.iter().map(|l| l.price).reduce(|a, b| a.min(b))
    }

    pub fn best_bid(&self) -> Option<Decimal> {
        self.bids.iter().map(|l| l.price).reduce(|a, b| a.max(b))
    }

    pub fn best_ask_size(&self) -> Option<Decimal> {
        let best = self.best_ask()?;
        self.asks
            .iter()
            .find(|l| l.price == best)
            .map(|l| l.size)
    }

    pub fn best_bid_size(&self) -> Option<Decimal> {
        let best = self.best_bid()?;
        self.bids
            .iter()
            .find(|l| l.price == best)
            .map(|l| l.size)
    }

    /// Total available size within `max_slippage` of the best ask.
    pub fn depth_at_ask(&self, max_slippage: Decimal) -> Decimal {
        let Some(best) = self.best_ask() else {
            return Decimal::ZERO;
        };
        let limit = best * (Decimal::ONE + max_slippage);
        self.asks
            .iter()
            .filter(|l| l.price <= limit)
            .map(|l| l.size)
            .sum()
    }
}

// ── Market Snapshot (polling scanner) ────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub market: Market,
    pub yes_orderbook: OrderBook,
    pub no_orderbook: OrderBook,
}

impl MarketSnapshot {
    pub fn yes_best_ask(&self) -> Option<Decimal> {
        self.yes_orderbook.best_ask()
    }
    pub fn no_best_ask(&self) -> Option<Decimal> {
        self.no_orderbook.best_ask()
    }
    pub fn combined_ask(&self) -> Option<Decimal> {
        Some(self.yes_best_ask()? + self.no_best_ask()?)
    }
    pub fn arbitrage_spread(&self) -> Option<Decimal> {
        Some(Decimal::ONE - self.combined_ask()?)
    }
    pub fn min_liquidity_at_ask(&self) -> Option<Decimal> {
        let y = self.yes_orderbook.best_ask_size()?;
        let n = self.no_orderbook.best_ask_size()?;
        Some(y.min(n))
    }
}

// ── Arbitrage Opportunity ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArbitrageOpportunity {
    pub market: Market,
    pub yes_ask: Decimal,
    pub no_ask: Decimal,
    pub combined_cost: Decimal,
    /// profit_pct = 1 - combined_cost  (e.g. 0.02 = 2%)
    pub profit_pct: Decimal,
    pub yes_size_available: Decimal,
    pub no_size_available: Decimal,
    /// Shares to trade (limited by liquidity, risk, and position size)
    pub max_trade_size: Decimal,
    pub detected_at: DateTime<Utc>,
}

impl ArbitrageOpportunity {
    pub fn gross_profit_usd(&self) -> Decimal {
        // payout is max_trade_size * $1, cost is max_trade_size * combined_cost
        self.max_trade_size * self.profit_pct
    }
}

// ── Order ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl From<OrderSide> for u8 {
    fn from(s: OrderSide) -> u8 {
        match s {
            OrderSide::Buy => 0,
            OrderSide::Sell => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub salt: u64,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    /// USDC amount in 6-decimal integer (e.g. $1.00 = 1_000_000)
    pub maker_amount: u128,
    /// Outcome tokens in integer (Polymarket uses 6 decimals too)
    pub taker_amount: u128,
    pub expiration: u64,
    pub nonce: u64,
    pub fee_rate_bps: u64,
    pub side: OrderSide,
    /// 0 = EOA, 1 = Poly proxy, 2 = Contract
    pub signature_type: u8,
    pub signature: Option<String>,
}

// ── Execution Result ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExecutionStatus {
    Filled,
    PartialFill,
    Cancelled,
    Failed,
    DryRun,
}

impl std::fmt::Display for ExecutionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ExecutionStatus::Filled => "FILLED",
            ExecutionStatus::PartialFill => "PARTIAL_FILL",
            ExecutionStatus::Cancelled => "CANCELLED",
            ExecutionStatus::Failed => "FAILED",
            ExecutionStatus::DryRun => "DRY_RUN",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionResult {
    pub status: ExecutionStatus,
    pub yes_order_id: Option<String>,
    pub no_order_id: Option<String>,
    pub expected_profit: Decimal,
    pub actual_cost: Decimal,
    pub executed_at: DateTime<Utc>,
    pub latency_ms: Option<u64>,
}

// ── Gamma API raw response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GammaToken {
    pub token_id: String,
    pub outcome: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GammaMarket {
    pub id: String,
    pub question: String,
    pub tokens: Vec<GammaToken>,
    #[serde(default)]
    pub volume: Option<serde_json::Value>,
    #[serde(default)]
    pub liquidity: Option<serde_json::Value>,
    pub end_date: Option<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub closed: bool,
    #[serde(default, rename = "negRisk")]
    pub neg_risk: bool,
    pub condition_id: Option<String>,
}

impl TryFrom<GammaMarket> for Market {
    type Error = crate::error::BotError;

    fn try_from(g: GammaMarket) -> crate::error::Result<Self> {
        // Find YES and NO tokens
        let yes_token = g
            .tokens
            .iter()
            .find(|t| t.outcome.to_lowercase() == "yes")
            .ok_or_else(|| crate::error::BotError::Api("No YES token in market".into()))?;
        let no_token = g
            .tokens
            .iter()
            .find(|t| t.outcome.to_lowercase() == "no")
            .ok_or_else(|| crate::error::BotError::Api("No NO token in market".into()))?;

        let parse_decimal = |v: &Option<serde_json::Value>| -> Decimal {
            match v {
                Some(serde_json::Value::String(s)) => {
                    Decimal::from_str(s).unwrap_or(Decimal::ZERO)
                }
                Some(serde_json::Value::Number(n)) => {
                    Decimal::from_str(&n.to_string()).unwrap_or(Decimal::ZERO)
                }
                _ => Decimal::ZERO,
            }
        };

        let end_date = g.end_date.and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(&s)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
                .or_else(|| {
                    // Try without timezone
                    chrono::NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S")
                        .ok()
                        .map(|ndt| ndt.and_utc())
                })
        });

        Ok(Market {
            id: g.id,
            question: g.question,
            yes_token: Token {
                token_id: yes_token.token_id.clone(),
                outcome: yes_token.outcome.clone(),
            },
            no_token: Token {
                token_id: no_token.token_id.clone(),
                outcome: no_token.outcome.clone(),
            },
            volume: parse_decimal(&g.volume),
            liquidity: parse_decimal(&g.liquidity),
            end_date,
            active: g.active,
            closed: g.closed,
            neg_risk: g.neg_risk,
            condition_id: g.condition_id,
        })
    }
}

// ── CLOB API raw response types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ClobPriceLevel {
    pub price: String,
    pub size: String,
}

#[derive(Debug, Deserialize)]
pub struct ClobOrderBook {
    pub market: Option<String>,
    pub asset_id: String,
    #[serde(default)]
    pub bids: Vec<ClobPriceLevel>,
    #[serde(default)]
    pub asks: Vec<ClobPriceLevel>,
}

impl TryFrom<ClobOrderBook> for OrderBook {
    type Error = crate::error::BotError;

    fn try_from(c: ClobOrderBook) -> crate::error::Result<Self> {
        let parse_levels = |levels: Vec<ClobPriceLevel>| -> Vec<PriceLevel> {
            levels
                .into_iter()
                .filter_map(|l| {
                    let price = Decimal::from_str(&l.price).ok()?;
                    let size = Decimal::from_str(&l.size).ok()?;
                    Some(PriceLevel { price, size })
                })
                .collect()
        };

        Ok(OrderBook {
            asset_id: c.asset_id,
            bids: parse_levels(c.bids),
            asks: parse_levels(c.asks),
        })
    }
}

// ── WebSocket message types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum WsMessage {
    Book(WsBookUpdate),
    PriceChange(WsPriceChange),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct WsBookUpdate {
    pub asset_id: String,
    #[serde(default)]
    pub bids: Vec<WsPriceLevel>,
    #[serde(default)]
    pub asks: Vec<WsPriceLevel>,
    pub best_bid: Option<String>,
    pub best_ask: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WsPriceChange {
    pub asset_id: String,
    pub side: String,
    pub price: String,
    pub size: String,
    pub best_bid: Option<String>,
    pub best_ask: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WsPriceLevel {
    pub price: String,
    pub size: String,
}

// ── Trade record (for database) ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub id: Option<i64>,
    pub timestamp: DateTime<Utc>,
    pub market: String,
    pub yes_ask: Decimal,
    pub no_ask: Decimal,
    pub combined_cost: Decimal,
    pub profit_pct: Decimal,
    pub trade_size: Decimal,
    pub expected_profit: Decimal,
    pub status: String,
    pub yes_order_id: Option<String>,
    pub no_order_id: Option<String>,
    pub latency_ms: Option<i64>,
    pub dry_run: bool,
}

// ── Backtest types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoricalPrice {
    pub timestamp: DateTime<Utc>,
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub volume: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestTrade {
    pub timestamp: DateTime<Utc>,
    pub market: String,
    pub yes_price: Decimal,
    pub no_price: Decimal,
    pub combined: Decimal,
    pub profit_pct: Decimal,
    pub trade_size: Decimal,
    pub gross_profit: Decimal,
    /// Fee = combined_cost * fee_rate
    pub fee: Decimal,
    pub net_profit: Decimal,
    pub filled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestResult {
    pub start_date: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
    pub initial_capital: Decimal,
    pub final_capital: Decimal,
    pub total_return_pct: Decimal,
    pub total_trades: u64,
    pub winning_trades: u64,
    pub losing_trades: u64,
    pub win_rate: Decimal,
    pub avg_profit_per_trade: Decimal,
    pub total_gross_profit: Decimal,
    pub total_fees: Decimal,
    pub total_net_profit: Decimal,
    pub max_drawdown_pct: Decimal,
    pub sharpe_ratio: Decimal,
    pub avg_profit_pct: Decimal,
    pub best_trade_profit: Decimal,
    pub worst_trade_loss: Decimal,
    pub trades: Vec<BacktestTrade>,
}

impl BacktestResult {
    pub fn print_summary(&self) {
        println!("\n═══════════════════════════════════════════════════");
        println!("           BACKTEST RESULTS SUMMARY");
        println!("═══════════════════════════════════════════════════");
        println!(
            "  Period:        {} → {}",
            self.start_date.format("%Y-%m-%d"),
            self.end_date.format("%Y-%m-%d")
        );
        println!("  Initial:       ${:.2}", self.initial_capital);
        println!("  Final:         ${:.2}", self.final_capital);
        println!("  Total Return:  {:.2}%", self.total_return_pct);
        println!("───────────────────────────────────────────────────");
        println!("  Total Trades:  {}", self.total_trades);
        println!("  Win Rate:      {:.1}%", self.win_rate * rust_decimal::Decimal::from(100));
        println!("  Avg Profit:    {:.4}% per trade", self.avg_profit_pct * rust_decimal::Decimal::from(100));
        println!("  Total Gross:   ${:.2}", self.total_gross_profit);
        println!("  Total Fees:    ${:.2}", self.total_fees);
        println!("  Total Net:     ${:.2}", self.total_net_profit);
        println!("───────────────────────────────────────────────────");
        println!("  Max Drawdown:  {:.2}%", self.max_drawdown_pct);
        println!("  Sharpe Ratio:  {:.3}", self.sharpe_ratio);
        println!("  Best Trade:    ${:.2}", self.best_trade_profit);
        println!("  Worst Trade:   ${:.2}", self.worst_trade_loss);
        println!("═══════════════════════════════════════════════════\n");
    }
}
