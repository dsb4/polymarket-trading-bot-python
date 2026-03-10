/// Main bot orchestration.
///
/// Audit improvements vs Python:
/// - Balance deduction uses a single Mutex-protected Decimal — no separate
///   _balance_lock + _execution_lock needed; one lock covers both.
/// - Circuit-breaker check and balance deduction are inside the same lock guard,
///   eliminating the TOCTOU race that existed in the Python version.
/// - Consecutive-loss circuit breaker re-checked before every execution.
use crate::{
    config::Config,
    db::Database,
    error::Result,
    executor::OrderExecutor,
    models::{ArbitrageOpportunity, ExecutionStatus, TradeRecord},
    risk::{CircuitBreakerResult, RiskManager},
    scanner::realtime::RealtimeScanner,
};
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

// Safety margin: only use 50% of displayed liquidity (matches Python)
const LIQUIDITY_SAFETY_MARGIN: Decimal = dec!(0.5);
// Minimum order value (Polymarket enforces $1 minimum per order)
const MIN_ORDER_VALUE: Decimal = dec!(1.10);
// Minimum shares floor
const MIN_SHARES_FLOOR: Decimal = dec!(5);

pub struct BotStats {
    pub started_at: chrono::DateTime<Utc>,
    pub opportunities_found: u64,
    pub trades_executed: u64,
    pub trades_successful: u64,
    pub total_profit: Decimal,
    pub scan_cycles: u64,
}

impl Default for BotStats {
    fn default() -> Self {
        Self {
            started_at: Utc::now(),
            opportunities_found: 0,
            trades_executed: 0,
            trades_successful: 0,
            total_profit: Decimal::ZERO,
            scan_cycles: 0,
        }
    }
}

pub struct RealtimeBot {
    config: Arc<Config>,
    executor: Arc<OrderExecutor>,
    risk: Arc<Mutex<RiskManager>>,
    db: Arc<Database>,
    /// Cached USDC balance (deducted eagerly, refreshed periodically).
    cached_balance: Arc<Mutex<Decimal>>,
    stats: Arc<Mutex<BotStats>>,
    running: Arc<AtomicBool>,
}

impl RealtimeBot {
    pub async fn new(config: Arc<Config>) -> Result<Self> {
        let db = Arc::new(
            Database::connect("sqlite:rarb.db")
                .await?,
        );

        let executor = Arc::new(OrderExecutor::new(Arc::clone(&config))?);
        let risk = Arc::new(Mutex::new(RiskManager::new((*config).clone())));

        Ok(Self {
            db,
            executor,
            risk,
            config,
            cached_balance: Arc::new(Mutex::new(Decimal::ZERO)),
            stats: Arc::new(Mutex::new(BotStats::default())),
            running: Arc::new(AtomicBool::new(false)),
        })
    }

    pub async fn run(&self) -> Result<()> {
        let mode = if self.config.dry_run { "DRY RUN" } else { "LIVE" };
        info!(
            "Starting REAL-TIME arbitrage bot [{}] min_profit={:.1}% max_position=${}",
            mode,
            self.config.min_profit_threshold * 100.0,
            self.config.max_position_size,
        );

        self.running.store(true, Ordering::Relaxed);

        // Start background tasks
        if !self.config.dry_run {
            self.spawn_balance_refresh_loop();
        }
        self.spawn_stats_persist_loop();

        // Create opportunity channel
        let (opp_tx, mut opp_rx) = mpsc::unbounded_channel::<ArbitrageOpportunity>();

        // Start scanner in its own task
        let config = Arc::clone(&self.config);
        let running_clone = Arc::clone(&self.running);
        tokio::spawn(async move {
            let mut scanner = match RealtimeScanner::new(config, opp_tx) {
                Ok(s) => s,
                Err(e) => {
                    error!("Failed to create scanner: {}", e);
                    return;
                }
            };
            if let Err(e) = scanner.run().await {
                error!("Scanner error: {}", e);
                running_clone.store(false, Ordering::Relaxed);
            }
        });

        // Process opportunities
        while let Some(opportunity) = opp_rx.recv().await {
            if !self.running.load(Ordering::Relaxed) {
                break;
            }
            self.handle_opportunity(opportunity).await;
        }

        self.shutdown().await;
        Ok(())
    }

    async fn handle_opportunity(&self, mut opp: ArbitrageOpportunity) {
        {
            let mut stats = self.stats.lock().await;
            stats.opportunities_found += 1;
        }

        let detection_ts_ms = opp.detected_at.timestamp_millis() as u64;

        // --- Risk: paused? ---
        let paused = {
            let mut risk = self.risk.lock().await;
            risk.is_paused()
        };
        if paused {
            return;
        }

        // --- Pre-trade time filter ---
        let seconds_until_resolution = opp.market.end_date.map(|end| {
            (end - Utc::now()).num_seconds() as f64
        });

        let filter = {
            let risk = self.risk.lock().await;
            risk.pre_trade_filters(seconds_until_resolution, None, None, None)
        };
        if !filter.allowed {
            info!("Pre-trade filter: {}", filter.reason);
            return;
        }

        // --- Liquidity check ---
        let raw_available = opp.yes_size_available.min(opp.no_size_available);
        let available_size = (raw_available * LIQUIDITY_SAFETY_MARGIN)
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);

        // Minimum shares so both orders are >= $1
        let min_shares_yes = (MIN_ORDER_VALUE / opp.yes_ask)
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
        let min_shares_no = (MIN_ORDER_VALUE / opp.no_ask)
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero);
        let min_required = min_shares_yes.max(min_shares_no).max(MIN_SHARES_FLOOR);

        if available_size < min_required {
            warn!(
                "Insufficient liquidity for {}: available={} min_required={}",
                &opp.market.question[..opp.market.question.len().min(40)],
                available_size,
                min_required,
            );
            let _ = self.db.insert_near_miss(
                &opp.market.question[..opp.market.question.len().min(60)],
                f64::try_from(opp.yes_ask).unwrap_or(0.0),
                f64::try_from(opp.no_ask).unwrap_or(0.0),
                f64::try_from(opp.combined_cost).unwrap_or(0.0),
                f64::try_from(opp.profit_pct).unwrap_or(0.0),
                f64::try_from(opp.yes_size_available).unwrap_or(0.0),
                f64::try_from(opp.no_size_available).unwrap_or(0.0),
                f64::try_from(min_required).unwrap_or(0.0),
                "insufficient_liquidity",
            ).await;
            return;
        }

        // --- Balance check + execution (inside single lock) ---
        let mut balance = self.cached_balance.lock().await;
        let mut risk = self.risk.lock().await;

        // Circuit breaker check
        match risk.check_circuit_breakers(*balance, None) {
            CircuitBreakerResult::Allowed => {}
            CircuitBreakerResult::Paused { reason, .. } => {
                warn!("Circuit breaker pause: {}", reason);
                return;
            }
            CircuitBreakerResult::Halted { reason } => {
                error!("Circuit breaker HALT: {} — stopping bot", reason);
                self.running.store(false, Ordering::Relaxed);
                return;
            }
        }

        // Position size
        let (risk_shares, _risk_usd) = risk.position_size(*balance, opp.yes_ask);
        let max_pos_shares = (Decimal::try_from(self.config.max_position_size).unwrap()
            / opp.combined_cost)
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);

        let mut trade_size = available_size.min(risk_shares).min(max_pos_shares)
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);

        if trade_size < min_required {
            warn!(
                "Risk-based size {} below minimum {} for {}",
                trade_size,
                min_required,
                &opp.market.question[..opp.market.question.len().min(40)],
            );
            return;
        }

        let required_cost = trade_size * opp.combined_cost;

        // Balance check
        if *balance < required_cost {
            // Try to fit in available balance
            let max_affordable = (*balance / opp.combined_cost)
                .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);
            if max_affordable >= min_required {
                trade_size = trade_size.min(max_affordable);
            } else {
                warn!(
                    "Insufficient balance for {}: need=${:.2} have=${:.2}",
                    &opp.market.question[..opp.market.question.len().min(40)],
                    required_cost,
                    balance,
                );
                return;
            }
        }

        // Deduct balance eagerly (prevents over-trading before response arrives)
        let required_cost = trade_size * opp.combined_cost;
        *balance -= required_cost;
        opp.max_trade_size = trade_size;

        drop(balance);
        drop(risk);

        // --- Execute ---
        {
            let mut stats = self.stats.lock().await;
            stats.trades_executed += 1;
        }

        match self.executor.execute(&opp, Some(detection_ts_ms)).await {
            Ok(result) => {
                let success = result.status == ExecutionStatus::Filled
                    || result.status == ExecutionStatus::DryRun;

                {
                    let mut risk = self.risk.lock().await;
                    risk.record_trade(success, result.expected_profit);
                }

                if success {
                    let mut stats = self.stats.lock().await;
                    stats.trades_successful += 1;
                    stats.total_profit += result.expected_profit;
                }

                // Persist trade record
                let record = TradeRecord {
                    id: None,
                    timestamp: result.executed_at,
                    market: opp.market.question[..opp.market.question.len().min(60)].to_string(),
                    yes_ask: opp.yes_ask,
                    no_ask: opp.no_ask,
                    combined_cost: opp.combined_cost,
                    profit_pct: opp.profit_pct,
                    trade_size: opp.max_trade_size,
                    expected_profit: result.expected_profit,
                    status: result.status.to_string(),
                    yes_order_id: result.yes_order_id.clone(),
                    no_order_id: result.no_order_id.clone(),
                    latency_ms: result.latency_ms.map(|ms| ms as i64),
                    dry_run: self.config.dry_run,
                };
                let _ = self.db.insert_trade(&record).await;

                if !success {
                    // Refresh balance from chain after failed trade
                    warn!(
                        "Trade not filled (status={}), will refresh balance on next tick",
                        result.status
                    );
                }
            }
            Err(e) => {
                {
                    let mut risk = self.risk.lock().await;
                    risk.record_trade(false, Decimal::ZERO);
                }
                error!("Execution error: {}", e);
            }
        }
    }

    fn spawn_balance_refresh_loop(&self) {
        // In live mode, periodically re-fetch actual USDC balance from the blockchain.
        // TODO: implement actual on-chain balance query via Polygon RPC.
        let balance = Arc::clone(&self.cached_balance);
        let running = Arc::clone(&self.running);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            while running.load(Ordering::Relaxed) {
                interval.tick().await;
                // Placeholder: actual implementation queries Polygon RPC for USDC.e balance
                // For now, log that this needs real implementation
                let _b = balance.lock().await;
                info!("Balance refresh tick (TODO: query Polygon RPC)");
            }
        });
    }

    fn spawn_stats_persist_loop(&self) {
        let stats = Arc::clone(&self.stats);
        let db = Arc::clone(&self.db);
        let running = Arc::clone(&self.running);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            while running.load(Ordering::Relaxed) {
                interval.tick().await;
                let s = stats.lock().await;
                let _ = db.upsert_stats(
                    0, // scanner markets (not available here)
                    0, // price updates
                    s.opportunities_found as i64,
                    s.trades_executed as i64,
                    s.trades_successful as i64,
                    &s.total_profit.to_string(),
                    true,
                ).await;
            }
        });
    }

    async fn shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.log_stats().await;
        info!("Bot shutdown complete");
    }

    async fn log_stats(&self) {
        let stats = self.stats.lock().await;
        let runtime = Utc::now() - stats.started_at;
        let hours = runtime.num_seconds() as f64 / 3600.0;
        info!(
            "Final stats: runtime={:.1}h opportunities={} trades={} successful={} profit=${}",
            hours,
            stats.opportunities_found,
            stats.trades_executed,
            stats.trades_successful,
            stats.total_profit,
        );
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        info!("Stop requested");
    }
}
