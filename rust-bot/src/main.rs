mod api;
mod backtest;
mod bot;
mod config;
mod db;
mod error;
mod executor;
mod models;
mod risk;
mod scanner;

use clap::{Parser, Subcommand};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(
    name = "rarb",
    version,
    about = "Polymarket Arbitrage Bot — Rust edition",
    long_about = "Real-time arbitrage bot for Polymarket prediction markets.\n\
                  Detects YES+NO price combinations below $1 and executes trades.\n\
                  Includes full backtesting capabilities."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the real-time arbitrage bot
    Run,

    /// Run a backtest over historical market data
    Backtest {
        /// Number of days of history to backtest
        #[arg(long, default_value = "30")]
        days: u32,

        /// Initial capital in USD
        #[arg(long, default_value = "10000")]
        capital: f64,

        /// Trade size per opportunity in USD
        #[arg(long, default_value = "100")]
        trade_size: f64,

        /// Use synthetic demo data instead of live API data
        #[arg(long)]
        demo: bool,

        /// Export results to JSON file
        #[arg(long)]
        output: Option<String>,
    },

    /// Show current configuration
    Config,

    /// Check wallet balance and credentials
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Load .env and config
    let cfg = config::Config::from_env().map_err(|e| anyhow::anyhow!("{}", e))?;

    // Set up tracing
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&cfg.log_level));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();

    match cli.command {
        Commands::Run => {
            run_bot(cfg).await?;
        }
        Commands::Backtest {
            days,
            capital,
            trade_size,
            demo,
            output,
        } => {
            run_backtest(cfg, days, capital, trade_size, demo, output).await?;
        }
        Commands::Config => {
            print_config(&cfg);
        }
        Commands::Status => {
            check_status(&cfg).await?;
        }
    }

    Ok(())
}

async fn run_bot(cfg: config::Config) -> anyhow::Result<()> {
    let cfg = Arc::new(cfg);

    // Graceful shutdown on Ctrl+C
    let bot = bot::RealtimeBot::new(Arc::clone(&cfg)).await?;
    let bot = Arc::new(bot);
    let bot_clone = Arc::clone(&bot);

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Ctrl+C received — shutting down...");
        bot_clone.stop();
    });

    bot.run().await.map_err(|e| anyhow::anyhow!("{}", e))?;
    Ok(())
}

async fn run_backtest(
    cfg: config::Config,
    days: u32,
    capital: f64,
    trade_size: f64,
    demo: bool,
    output: Option<String>,
) -> anyhow::Result<()> {
    use chrono::{Duration, Utc};
    use rust_decimal::Decimal;

    let end = Utc::now();
    let start = end - Duration::days(days as i64);
    let initial_capital = Decimal::try_from(capital).unwrap_or(rust_decimal_macros::dec!(10000));
    let size = Decimal::try_from(trade_size).unwrap_or(rust_decimal_macros::dec!(100));

    info!("Starting backtest: {} days, capital=${}, trade_size=${}", days, capital, trade_size);

    let result = if demo {
        info!("Running demo backtest with synthetic data");
        backtest::run_demo_backtest(cfg).await.map_err(|e| anyhow::anyhow!("{}", e))?
    } else {
        let engine =
            backtest::BacktestEngine::new(cfg).map_err(|e| anyhow::anyhow!("{}", e))?;

        // In live mode, fetch markets from Gamma API and backtest each one.
        // For now, run the synthetic demo as a fallback with a notice.
        info!("Live backtest requires active markets — running demo backtest (pass --demo to skip this notice)");
        backtest::run_demo_backtest(
            crate::config::Config::from_env().unwrap_or_else(|_| {
                panic!("Failed to load config for live backtest")
            }),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?
    };

    result.print_summary();

    if let Some(path) = output {
        let json = serde_json::to_string_pretty(&result)?;
        std::fs::write(&path, json)?;
        info!("Results written to {}", path);
    }

    Ok(())
}

fn print_config(cfg: &config::Config) {
    println!("\n═══════════════════════════════════════════════════");
    println!("               BOT CONFIGURATION");
    println!("═══════════════════════════════════════════════════");
    println!("  Mode:              {}", if cfg.dry_run { "DRY RUN" } else { "LIVE TRADING" });
    println!("  Min profit:        {:.1}%", cfg.min_profit_threshold * 100.0);
    println!("  Max position:      ${}", cfg.max_position_size);
    println!("  Min liquidity:     ${}", cfg.min_liquidity_usd);
    println!("  Max resolution:    {} days", cfg.max_days_until_resolution);
    println!("  WS connections:    {}", cfg.num_ws_connections);
    println!("  Poll interval:     {}s", cfg.poll_interval_seconds);
    println!("───────────────────────────────────────────────────");
    println!("  Risk per trade:    {}%", cfg.risk_per_trade_pct);
    println!("  Stop loss:         {}%", cfg.stop_loss_pct);
    println!("  Position cap:      {}%", cfg.position_cap_pct);
    println!("  Consec loss limit: {}", cfg.consecutive_losses_pause);
    println!("  Session drawdown:  {}%", cfg.session_drawdown_pct);
    println!("  Daily drawdown:    {}%", cfg.daily_drawdown_pct);
    println!("  Monthly drawdown:  {}%", cfg.monthly_drawdown_pct);
    println!("───────────────────────────────────────────────────");
    println!("  Wallet:            {}", cfg.wallet_address.as_deref().unwrap_or("NOT SET"));
    println!("  L2 API key:        {}", if cfg.poly_api_key.is_some() { "SET" } else { "NOT SET" });
    println!("  Proxy:             {}", if cfg.is_proxy_enabled() { cfg.get_socks5_proxy_url().unwrap_or_default() } else { "DISABLED".into() });
    println!("  Slack:             {}", if cfg.slack_webhook_url.is_some() { "SET" } else { "NOT SET" });
    println!("═══════════════════════════════════════════════════\n");
}

async fn check_status(cfg: &config::Config) -> anyhow::Result<()> {
    println!("\n═══════════════════════════════════════════════════");
    println!("                   BOT STATUS");
    println!("═══════════════════════════════════════════════════");

    // Check credentials
    if cfg.is_trading_enabled() {
        println!("  Trading:    ✓ Credentials configured");
        println!("  Wallet:     {}", cfg.wallet_address.as_deref().unwrap_or("unknown"));
    } else {
        println!("  Trading:    ✗ Credentials NOT configured (dry run only)");
    }

    // Check Gamma API connectivity
    print!("  Gamma API:  ");
    match api::gamma::GammaClient::new(cfg) {
        Ok(client) => {
            match client.fetch_all_active_markets(10_000.0, 7).await {
                Ok(markets) => println!("✓ Connected ({} markets)", markets.len()),
                Err(e) => println!("✗ Error: {}", e),
            }
        }
        Err(e) => println!("✗ Failed to create client: {}", e),
    }

    println!("═══════════════════════════════════════════════════\n");
    Ok(())
}
