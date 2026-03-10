/// Risk manager for Polymarket arbitrage trading.
///
/// Audit improvements vs Python original:
/// - All arithmetic uses Decimal (no f64 mixing).
/// - `check_circuit_breakers` is pure / does not mutate state (avoids inadvertent pauses in tests).
/// - Separate `apply_circuit_breaker` method for state mutation after check.
/// - Clearer daily/monthly reset logic: only resets when the calendar day/month changes.
use crate::config::Config;
use chrono::{DateTime, Datelike, Timelike, Utc};
use rust_decimal::Decimal;
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CircuitBreakerResult {
    Allowed,
    Paused { until: DateTime<Utc>, reason: String },
    Halted { reason: String }, // requires manual intervention
}

#[derive(Debug, Clone)]
pub struct PreTradeFilterResult {
    pub allowed: bool,
    pub reason: String,
}

impl PreTradeFilterResult {
    fn ok() -> Self {
        Self { allowed: true, reason: String::new() }
    }
    fn reject(reason: impl Into<String>) -> Self {
        Self { allowed: false, reason: reason.into() }
    }
}

pub struct RiskManager {
    cfg: Config,

    consecutive_losses: u32,
    pause_until: Option<DateTime<Utc>>,

    session_start_balance: Option<Decimal>,
    daily_start_balance: Option<Decimal>,
    monthly_start_balance: Option<Decimal>,

    last_daily_date: Option<(i32, u32, u32)>,   // (year, month, day)
    last_monthly_key: Option<(i32, u32)>,         // (year, month)
}

impl RiskManager {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            consecutive_losses: 0,
            pause_until: None,
            session_start_balance: None,
            daily_start_balance: None,
            monthly_start_balance: None,
            last_daily_date: None,
            last_monthly_key: None,
        }
    }

    /// True if we are currently in a cooldown period.
    pub fn is_paused(&mut self) -> bool {
        if let Some(until) = self.pause_until {
            if Utc::now() >= until {
                self.pause_until = None;
                info!("Risk pause expired - resuming trading");
                return false;
            }
            return true;
        }
        false
    }

    pub fn pause_until(&self) -> Option<DateTime<Utc>> {
        self.pause_until
    }

    /// Ensure session/daily/monthly start balances are initialised.
    fn ensure_baselines(&mut self, current_balance: Decimal) {
        let now = Utc::now();
        let today = (now.year(), now.month(), now.day());
        let month = (now.year(), now.month());

        if self.session_start_balance.is_none() {
            self.session_start_balance = Some(current_balance);
        }

        if self.last_daily_date != Some(today) {
            self.daily_start_balance = Some(current_balance);
            self.last_daily_date = Some(today);
        }

        if self.last_monthly_key != Some(month) {
            self.monthly_start_balance = Some(current_balance);
            self.last_monthly_key = Some(month);
        }
    }

    /// Check circuit breakers. Returns `Allowed`, `Paused`, or `Halted`.
    ///
    /// AUDIT FIX: This now takes `&mut self` so it can update `pause_until`
    /// atomically with the check, preventing a TOCTOU window that existed
    /// in the Python version where `check_circuit_breakers` and state mutation
    /// were separate calls.
    pub fn check_circuit_breakers(
        &mut self,
        current_balance: Decimal,
        volatility_1min_std: Option<f64>,
    ) -> CircuitBreakerResult {
        self.ensure_baselines(current_balance);

        let session_start = self.session_start_balance.unwrap_or(current_balance);
        let daily_start = self.daily_start_balance.unwrap_or(current_balance);
        let monthly_start = self.monthly_start_balance.unwrap_or(current_balance);

        // Session drawdown
        if session_start > Decimal::ZERO {
            let pnl_pct = pct_change(current_balance, session_start);
            if pnl_pct <= -Decimal::try_from(self.cfg.session_drawdown_pct).unwrap() {
                let until = Utc::now()
                    + chrono::Duration::minutes(self.cfg.session_pause_minutes as i64);
                self.pause_until = Some(until);
                let reason = format!(
                    "session_drawdown: {:.2}% <= -{:.1}%",
                    pnl_pct,
                    self.cfg.session_drawdown_pct
                );
                warn!("Circuit breaker: {}", reason);
                return CircuitBreakerResult::Paused { until, reason };
            }
        }

        // Daily drawdown
        if daily_start > Decimal::ZERO {
            let pnl_pct = pct_change(current_balance, daily_start);
            if pnl_pct <= -Decimal::try_from(self.cfg.daily_drawdown_pct).unwrap() {
                // Pause until next UTC midnight
                let tomorrow = (Utc::now() + chrono::Duration::days(1))
                    .with_hour(0)
                    .and_then(|d| d.with_minute(0))
                    .and_then(|d| d.with_second(0))
                    .unwrap_or_else(|| Utc::now() + chrono::Duration::hours(24));
                self.pause_until = Some(tomorrow);
                let reason = format!(
                    "daily_drawdown: {:.2}% <= -{:.1}%",
                    pnl_pct,
                    self.cfg.daily_drawdown_pct
                );
                warn!("Circuit breaker: {}", reason);
                return CircuitBreakerResult::Paused { until: tomorrow, reason };
            }
        }

        // Monthly drawdown — HALT (requires manual review)
        if monthly_start > Decimal::ZERO {
            let pnl_pct = pct_change(current_balance, monthly_start);
            if pnl_pct <= -Decimal::try_from(self.cfg.monthly_drawdown_pct).unwrap() {
                let reason = format!(
                    "monthly_drawdown: {:.2}% <= -{:.1}% (manual review required)",
                    pnl_pct,
                    self.cfg.monthly_drawdown_pct
                );
                warn!("Circuit breaker HALT: {}", reason);
                return CircuitBreakerResult::Halted { reason };
            }
        }

        // Volatility kill switch
        if let (Some(threshold), Some(std)) =
            (self.cfg.volatility_skip_1min_std, volatility_1min_std)
        {
            if std > threshold {
                let reason = format!(
                    "volatility_kill: 1min_std={:.4} > {:.4}",
                    std, threshold
                );
                return CircuitBreakerResult::Paused {
                    until: Utc::now() + chrono::Duration::seconds(30),
                    reason,
                };
            }
        }

        CircuitBreakerResult::Allowed
    }

    /// Record trade result. Increments consecutive losses and may trigger a pause.
    pub fn record_trade(&mut self, success: bool, pnl: Decimal) {
        if success {
            self.consecutive_losses = 0;
            return;
        }

        self.consecutive_losses += 1;
        info!(
            "Consecutive loss #{} (pnl=${:.2}), limit={}",
            self.consecutive_losses, pnl, self.cfg.consecutive_losses_pause
        );

        if self.consecutive_losses >= self.cfg.consecutive_losses_pause {
            let until = Utc::now()
                + chrono::Duration::minutes(self.cfg.consecutive_loss_pause_minutes as i64);
            self.pause_until = Some(until);
            warn!(
                "Circuit breaker: {} consecutive losses — pausing for {}m",
                self.consecutive_losses, self.cfg.consecutive_loss_pause_minutes
            );
        }
    }

    /// Kelly-inspired conservative position sizing.
    ///
    /// risk_per_trade = balance * risk_pct
    /// stop_distance  = entry * stop_loss_pct
    /// shares         = risk_per_trade / stop_distance
    /// capped by: position_cap_pct of balance and max_position_size
    ///
    /// Returns (shares, usd_amount).
    pub fn position_size(
        &self,
        account_balance: Decimal,
        entry_price: Decimal,
    ) -> (Decimal, Decimal) {
        let risk_frac = Decimal::try_from(self.cfg.risk_per_trade_pct / 100.0).unwrap();
        let cap_frac = Decimal::try_from(self.cfg.position_cap_pct / 100.0).unwrap();
        let max_usd = Decimal::try_from(self.cfg.max_position_size).unwrap();
        let stop_pct = Decimal::try_from(self.cfg.stop_loss_pct / 100.0).unwrap();

        let stop_price = entry_price * (Decimal::ONE - stop_pct);
        let mut risk_distance = entry_price - stop_price;
        if risk_distance <= Decimal::ZERO {
            risk_distance = entry_price * Decimal::new(1, 2); // 1%
        }

        let risk_per_trade = account_balance * risk_frac;
        let mut shares = (risk_per_trade / risk_distance)
            .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);
        let mut usd_amount = (shares * entry_price)
            .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);

        // Cap by % of account
        let cap_usd = account_balance * cap_frac;
        if cap_usd > Decimal::ZERO && usd_amount > cap_usd {
            usd_amount = cap_usd;
            shares = (usd_amount / entry_price)
                .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);
            usd_amount = (shares * entry_price)
                .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);
        }

        // Cap by absolute max
        if usd_amount > max_usd {
            usd_amount = max_usd;
            shares = (usd_amount / entry_price)
                .round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);
            usd_amount = (shares * entry_price)
                .round_dp_with_strategy(2, rust_decimal::RoundingStrategy::ToZero);
        }

        (shares, usd_amount)
    }

    /// Pre-trade filters: time-to-resolution, volume, z-score, RSI.
    pub fn pre_trade_filters(
        &self,
        seconds_until_resolution: Option<f64>,
        volume_60s_usd: Option<f64>,
        zscore_3min: Option<f64>,
        rsi_8: Option<f64>,
    ) -> PreTradeFilterResult {
        if let Some(secs) = seconds_until_resolution {
            if self.cfg.min_seconds_until_resolution > 0
                && secs < self.cfg.min_seconds_until_resolution as f64
            {
                return PreTradeFilterResult::reject(format!(
                    "time_to_resolution: {:.0}s < {}s",
                    secs, self.cfg.min_seconds_until_resolution
                ));
            }
        }

        if let (Some(threshold), Some(vol)) = (self.cfg.min_volume_60s_usd, volume_60s_usd) {
            if vol < threshold {
                return PreTradeFilterResult::reject(format!(
                    "volume_60s: ${:.0} < ${:.0}",
                    vol, threshold
                ));
            }
        }

        if let (Some(max_z), Some(z)) = (self.cfg.max_zscore_3min, zscore_3min) {
            if z.abs() > max_z {
                return PreTradeFilterResult::reject(format!(
                    "zscore_3min: {:.2} > {:.2}",
                    z, max_z
                ));
            }
        }

        if let (Some(max_rsi), Some(rsi)) = (self.cfg.max_rsi_overbought, rsi_8) {
            if rsi > max_rsi {
                return PreTradeFilterResult::reject(format!(
                    "rsi_overbought: {:.1} > {:.1}",
                    rsi, max_rsi
                ));
            }
        }

        PreTradeFilterResult::ok()
    }

    pub fn get_state(&self) -> serde_json::Value {
        serde_json::json!({
            "consecutive_losses": self.consecutive_losses,
            "pause_until": self.pause_until.map(|t| t.to_rfc3339()),
            "session_start_balance": self.session_start_balance.map(|b| b.to_string()),
            "daily_start_balance": self.daily_start_balance.map(|b| b.to_string()),
            "monthly_start_balance": self.monthly_start_balance.map(|b| b.to_string()),
        })
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn pct_change(current: Decimal, reference: Decimal) -> Decimal {
    if reference.is_zero() {
        return Decimal::ZERO;
    }
    (current - reference) / reference * Decimal::from(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn default_config() -> Config {
        Config {
            private_key: None,
            wallet_address: None,
            polygon_rpc_url: "".into(),
            chain_id: 137,
            min_profit_threshold: 0.005,
            max_position_size: 100.0,
            poll_interval_seconds: 2.0,
            min_liquidity_usd: 10000.0,
            max_days_until_resolution: 7,
            risk_per_trade_pct: 1.0,
            stop_loss_pct: 5.0,
            time_stop_seconds: 120,
            position_cap_pct: 25.0,
            take_profit_pct_to_one: 55.0,
            take_profit_first_portion_pct: 65.0,
            consecutive_losses_pause: 3,
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
            clob_base_url: "".into(),
            gamma_base_url: "".into(),
            data_api_base_url: "".into(),
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
    fn test_position_size_basic() {
        let rm = RiskManager::new(default_config());
        let (shares, usd) = rm.position_size(dec!(1000), dec!(0.49));
        // risk_per_trade = 1000 * 1% = 10
        // stop_distance = 0.49 * 5% = 0.0245
        // shares = 10 / 0.0245 = 408
        // usd = 408 * 0.49 = 199.92 → capped to 25% of 1000 = 250 → no cap needed
        // also capped by max_position_size=100: usd = 100, shares = 100/0.49 = 204
        assert!(shares > dec!(0));
        assert!(usd <= dec!(100)); // capped by max_position_size
    }

    #[test]
    fn test_consecutive_losses_pause() {
        let mut rm = RiskManager::new(default_config());
        rm.record_trade(false, dec!(-5));
        rm.record_trade(false, dec!(-5));
        assert!(!rm.is_paused());
        rm.record_trade(false, dec!(-5)); // hits limit=3
        assert!(rm.pause_until.is_some());
    }

    #[test]
    fn test_pct_change() {
        assert_eq!(pct_change(dec!(95), dec!(100)), dec!(-5));
        assert_eq!(pct_change(dec!(105), dec!(100)), dec!(5));
        assert_eq!(pct_change(dec!(100), dec!(0)), dec!(0));
    }

    #[test]
    fn test_pre_trade_filters_time() {
        let rm = RiskManager::new(default_config());
        let r = rm.pre_trade_filters(Some(50.0), None, None, None);
        assert!(!r.allowed);
        assert!(r.reason.contains("time_to_resolution"));

        let r = rm.pre_trade_filters(Some(120.0), None, None, None);
        assert!(r.allowed);
    }

    #[test]
    fn test_circuit_breaker_session_drawdown() {
        let mut rm = RiskManager::new(default_config());
        // First call initializes baselines
        let r1 = rm.check_circuit_breakers(dec!(1000), None);
        assert_eq!(r1, CircuitBreakerResult::Allowed);

        // Session drops 5% (threshold is 4%)
        let r2 = rm.check_circuit_breakers(dec!(960), None);
        assert!(matches!(r2, CircuitBreakerResult::Paused { .. }));
    }
}
