/// SQLite persistence layer using sqlx.
///
/// Tables:
///   - trades       - execution records
///   - alerts       - arbitrage alerts detected
///   - near_misses  - opportunities that were filtered out
///   - stats        - current scanner/bot stats (single row)
///   - stats_history - hourly snapshots
use crate::{error::Result, models::TradeRecord};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::{migrate::MigrateDatabase, sqlite::SqlitePool, Row, Sqlite};
use tracing::info;

pub struct Database {
    pool: SqlitePool,
}

impl Database {
    pub async fn connect(db_url: &str) -> Result<Self> {
        // Create database file if it doesn't exist
        if !Sqlite::database_exists(db_url).await.unwrap_or(false) {
            Sqlite::create_database(db_url).await?;
            info!("Created new SQLite database: {}", db_url);
        }

        let pool = SqlitePool::connect(db_url).await?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<()> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS trades (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp   TEXT    NOT NULL,
                market      TEXT    NOT NULL,
                yes_ask     TEXT    NOT NULL,
                no_ask      TEXT    NOT NULL,
                combined    TEXT    NOT NULL,
                profit_pct  TEXT    NOT NULL,
                trade_size  TEXT    NOT NULL,
                expected_profit TEXT NOT NULL,
                status      TEXT    NOT NULL,
                yes_order_id TEXT,
                no_order_id  TEXT,
                latency_ms  INTEGER,
                dry_run     INTEGER NOT NULL DEFAULT 1
            );

            CREATE TABLE IF NOT EXISTS alerts (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp   TEXT    NOT NULL,
                market      TEXT    NOT NULL,
                yes_ask     REAL    NOT NULL,
                no_ask      REAL    NOT NULL,
                combined    REAL    NOT NULL,
                profit_pct  REAL    NOT NULL,
                yes_liquidity REAL,
                no_liquidity  REAL,
                days_until_resolution INTEGER,
                resolution_date TEXT,
                first_seen  TEXT,
                duration_secs REAL,
                platform    TEXT    DEFAULT 'polymarket'
            );

            CREATE TABLE IF NOT EXISTS near_misses (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp   TEXT    NOT NULL,
                market      TEXT    NOT NULL,
                yes_ask     REAL    NOT NULL,
                no_ask      REAL    NOT NULL,
                combined    REAL    NOT NULL,
                profit_pct  REAL    NOT NULL,
                yes_liquidity REAL,
                no_liquidity  REAL,
                min_required REAL,
                reason      TEXT
            );

            CREATE TABLE IF NOT EXISTS bot_stats (
                id                  INTEGER PRIMARY KEY DEFAULT 1,
                markets             INTEGER DEFAULT 0,
                price_updates       INTEGER DEFAULT 0,
                arbitrage_alerts    INTEGER DEFAULT 0,
                trades_executed     INTEGER DEFAULT 0,
                trades_filled       INTEGER DEFAULT 0,
                total_profit        TEXT    DEFAULT '0',
                ws_connected        INTEGER DEFAULT 0,
                updated_at          TEXT    NOT NULL
            );

            CREATE TABLE IF NOT EXISTS stats_history (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp           TEXT    NOT NULL,
                hour                TEXT    NOT NULL,
                markets             INTEGER DEFAULT 0,
                price_updates       INTEGER DEFAULT 0,
                arbitrage_alerts    INTEGER DEFAULT 0,
                executions_attempted INTEGER DEFAULT 0,
                executions_filled   INTEGER DEFAULT 0,
                ws_connected        INTEGER DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS balance_history (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp       TEXT    NOT NULL,
                usdc_balance    REAL    NOT NULL,
                positions_value REAL    DEFAULT 0,
                total_usd       REAL    NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_trades_timestamp ON trades(timestamp);
            CREATE INDEX IF NOT EXISTS idx_alerts_timestamp ON alerts(timestamp);
            CREATE INDEX IF NOT EXISTS idx_stats_history_hour ON stats_history(hour);
            "#,
        )
        .execute(&self.pool)
        .await?;

        info!("Database schema ready");
        Ok(())
    }

    // ── Trade records ─────────────────────────────────────────────────────────

    pub async fn insert_trade(&self, trade: &TradeRecord) -> Result<i64> {
        let row = sqlx::query(
            r#"
            INSERT INTO trades
                (timestamp, market, yes_ask, no_ask, combined, profit_pct,
                 trade_size, expected_profit, status, yes_order_id, no_order_id,
                 latency_ms, dry_run)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(trade.timestamp.to_rfc3339())
        .bind(&trade.market)
        .bind(trade.yes_ask.to_string())
        .bind(trade.no_ask.to_string())
        .bind(trade.combined_cost.to_string())
        .bind(trade.profit_pct.to_string())
        .bind(trade.trade_size.to_string())
        .bind(trade.expected_profit.to_string())
        .bind(&trade.status)
        .bind(&trade.yes_order_id)
        .bind(&trade.no_order_id)
        .bind(trade.latency_ms)
        .bind(trade.dry_run as i32)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.get::<i64, _>("id"))
    }

    pub async fn get_recent_trades(&self, limit: i64) -> Result<Vec<serde_json::Value>> {
        let rows = sqlx::query(
            "SELECT * FROM trades ORDER BY timestamp DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.get::<i64, _>("id"),
                    "timestamp": r.get::<String, _>("timestamp"),
                    "market": r.get::<String, _>("market"),
                    "profit_pct": r.get::<String, _>("profit_pct"),
                    "trade_size": r.get::<String, _>("trade_size"),
                    "expected_profit": r.get::<String, _>("expected_profit"),
                    "status": r.get::<String, _>("status"),
                    "dry_run": r.get::<i32, _>("dry_run") != 0,
                })
            })
            .collect())
    }

    pub async fn get_total_profit(&self) -> Result<Decimal> {
        let row = sqlx::query(
            "SELECT COALESCE(SUM(CAST(expected_profit AS REAL)), 0) AS total FROM trades WHERE status IN ('FILLED','DRY_RUN')"
        )
        .fetch_one(&self.pool)
        .await?;
        let total: f64 = row.get("total");
        Ok(Decimal::try_from(total).unwrap_or(Decimal::ZERO))
    }

    // ── Alert records ─────────────────────────────────────────────────────────

    pub async fn insert_alert(
        &self,
        market: &str,
        yes_ask: f64,
        no_ask: f64,
        combined: f64,
        profit: f64,
        yes_liquidity: Option<f64>,
        no_liquidity: Option<f64>,
        days_until_resolution: Option<i32>,
        resolution_date: Option<&str>,
        first_seen: Option<&str>,
        duration_secs: Option<f64>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO alerts
                (timestamp, market, yes_ask, no_ask, combined, profit_pct,
                 yes_liquidity, no_liquidity, days_until_resolution,
                 resolution_date, first_seen, duration_secs)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(Utc::now().to_rfc3339())
        .bind(market)
        .bind(yes_ask)
        .bind(no_ask)
        .bind(combined)
        .bind(profit)
        .bind(yes_liquidity)
        .bind(no_liquidity)
        .bind(days_until_resolution)
        .bind(resolution_date)
        .bind(first_seen)
        .bind(duration_secs)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn update_alert_duration(&self, market: &str, duration_secs: f64) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE alerts SET duration_secs = ? WHERE market = ? AND duration_secs IS NULL ORDER BY timestamp DESC LIMIT 1"
        )
        .bind(duration_secs)
        .bind(market)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    // ── Near-miss records ─────────────────────────────────────────────────────

    pub async fn insert_near_miss(
        &self,
        market: &str,
        yes_ask: f64,
        no_ask: f64,
        combined: f64,
        profit_pct: f64,
        yes_liquidity: f64,
        no_liquidity: f64,
        min_required: f64,
        reason: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO near_misses
                (timestamp, market, yes_ask, no_ask, combined, profit_pct,
                 yes_liquidity, no_liquidity, min_required, reason)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(Utc::now().to_rfc3339())
        .bind(market)
        .bind(yes_ask)
        .bind(no_ask)
        .bind(combined)
        .bind(profit_pct)
        .bind(yes_liquidity)
        .bind(no_liquidity)
        .bind(min_required)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Stats ─────────────────────────────────────────────────────────────────

    pub async fn upsert_stats(
        &self,
        markets: i64,
        price_updates: i64,
        arb_alerts: i64,
        trades_executed: i64,
        trades_filled: i64,
        total_profit: &str,
        ws_connected: bool,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO bot_stats
                (id, markets, price_updates, arbitrage_alerts, trades_executed,
                 trades_filled, total_profit, ws_connected, updated_at)
            VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                markets = excluded.markets,
                price_updates = excluded.price_updates,
                arbitrage_alerts = excluded.arbitrage_alerts,
                trades_executed = excluded.trades_executed,
                trades_filled = excluded.trades_filled,
                total_profit = excluded.total_profit,
                ws_connected = excluded.ws_connected,
                updated_at = excluded.updated_at
            "#,
        )
        .bind(markets)
        .bind(price_updates)
        .bind(arb_alerts)
        .bind(trades_executed)
        .bind(trades_filled)
        .bind(total_profit)
        .bind(ws_connected as i32)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn insert_stats_history(
        &self,
        hour: &str,
        markets: i64,
        price_updates: i64,
        arb_alerts: i64,
        executed: i64,
        filled: i64,
        ws_connected: bool,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO stats_history
                (timestamp, hour, markets, price_updates, arbitrage_alerts,
                 executions_attempted, executions_filled, ws_connected)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(Utc::now().to_rfc3339())
        .bind(hour)
        .bind(markets)
        .bind(price_updates)
        .bind(arb_alerts)
        .bind(executed)
        .bind(filled)
        .bind(ws_connected as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ── Balance history ───────────────────────────────────────────────────────

    pub async fn insert_balance_snapshot(
        &self,
        usdc_balance: f64,
        positions_value: f64,
        total_usd: f64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO balance_history (timestamp, usdc_balance, positions_value, total_usd) VALUES (?, ?, ?, ?)"
        )
        .bind(Utc::now().to_rfc3339())
        .bind(usdc_balance)
        .bind(positions_value)
        .bind(total_usd)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
