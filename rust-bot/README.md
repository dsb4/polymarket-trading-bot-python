# rarb — Polymarket Arbitrage Bot (Rust)

Real-time arbitrage bot for [Polymarket](https://polymarket.com) prediction markets.
Detects YES + NO token combinations priced below $1.00 and executes both legs simultaneously, locking in a risk-free profit at resolution.

## How It Works

Every binary market on Polymarket has a YES token and a NO token. At resolution, exactly one pays $1 and the other pays $0 — so together they always pay $1. If you can buy both for less than $1, you profit regardless of outcome.

```
Profit = $1.00 − (YES_ask + NO_ask)

Example:
  YES ask = $0.47
  NO  ask = $0.49
  Combined = $0.96  →  $0.04 profit per share (4.2%)
```

The bot streams live orderbook data over WebSocket, detects these windows in microseconds, and submits signed orders to the Polymarket CLOB in parallel.

## Features

- **Real-time scanning** — multiple WebSocket connections covering thousands of markets simultaneously
- **Native EIP-712 signing** — no external wallet library; orders signed with `k256` + `sha3`
- **HMAC-SHA256 L2 auth** — Polymarket API authentication implemented natively
- **Kelly-inspired position sizing** — risk-per-trade as a fraction of account balance
- **Layered circuit breakers** — consecutive-loss pause, session/daily/monthly drawdown halts
- **Backtesting engine** — simulate the strategy over historical price data with slippage and fee modeling
- **SQLite persistence** — all trades, alerts, and stats stored locally
- **SOCKS5 proxy support** — route through a VPS to bypass US geo-restrictions
- **Dry-run mode** — full simulation without placing real orders

## Quick Start

### Prerequisites

- Rust 1.75+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- A Polymarket account with L2 API credentials

### Install

```bash
git clone <repo>
cd rust-bot
cp .env.example .env
# Edit .env with your credentials
```

### Run

```bash
# Dry run (no real orders)
cargo run --release -- run

# Backtest with synthetic data (no API key needed)
cargo run --release -- backtest --demo --days 30

# Check config and connectivity
cargo run --release -- config
cargo run --release -- status
```

## Configuration

Copy `.env.example` to `.env` and fill in your values.

| Variable | Default | Description |
|----------|---------|-------------|
| `PRIVATE_KEY` | — | Wallet private key (`0x` prefixed) |
| `WALLET_ADDRESS` | — | Wallet address (`0x` prefixed) |
| `POLY_API_KEY` | — | Polymarket L2 API key |
| `POLY_API_SECRET` | — | Polymarket L2 API secret (base64) |
| `POLY_API_PASSPHRASE` | — | Polymarket L2 API passphrase |
| `DRY_RUN` | `true` | Set `false` for live trading |
| `MIN_PROFIT_THRESHOLD` | `0.005` | Minimum 0.5% profit to trade |
| `MAX_POSITION_SIZE` | `100` | Max USD per trade |
| `MIN_LIQUIDITY_USD` | `10000` | Skip markets with <$10k liquidity |
| `MAX_DAYS_UNTIL_RESOLUTION` | `7` | Skip markets resolving >7 days out |
| `NUM_WS_CONNECTIONS` | `6` | WebSocket connections (250 markets each) |
| `RISK_PER_TRADE_PCT` | `0.8` | % of balance risked per trade |
| `CONSECUTIVE_LOSSES_PAUSE` | `5` | Pause after N consecutive losses |
| `SESSION_DRAWDOWN_PCT` | `4.0` | Pause after 4% session drawdown |
| `DAILY_DRAWDOWN_PCT` | `8.0` | Stop for the day after 8% drawdown |
| `MONTHLY_DRAWDOWN_PCT` | `20.0` | Halt bot after 20% monthly drawdown |
| `SOCKS5_PROXY_HOST` | — | Optional proxy host (US geo-bypass) |
| `LOG_LEVEL` | `INFO` | `DEBUG`, `INFO`, `WARN`, `ERROR` |

> **Fee note:** Polymarket charges ~1% per trade. Set `MIN_PROFIT_THRESHOLD` to at least `0.015` (1.5%) to clear fees and remain profitable.

## CLI Reference

```
rarb <COMMAND>

Commands:
  run        Run the real-time arbitrage bot
  backtest   Backtest the strategy over historical data
  config     Show current configuration
  status     Check wallet and API connectivity
  help       Print help

Backtest options:
  --days <N>          Days of history (default: 30)
  --capital <USD>     Starting capital (default: 10000)
  --trade-size <USD>  Per-trade size (default: 100)
  --demo              Use synthetic data (no API key needed)
  --output <FILE>     Write JSON results to file
```

## Backtesting

The backtester simulates the strategy on historical Polymarket price data:

```
════════════════════════════════════════════════════════
           BACKTEST RESULTS SUMMARY
════════════════════════════════════════════════════════
  Period:        2026-02-08 → 2026-03-10
  Initial:       $10000.00
  Final:         $10847.32
  Total Return:  8.47%
───────────────────────────────────────────────────────
  Total Trades:  412
  Win Rate:      94.2%
  Avg Profit:    0.0206% per trade
  Total Gross:   $923.44
  Total Fees:    $76.12
  Total Net:     $847.32
───────────────────────────────────────────────────────
  Max Drawdown:  1.83%
  Sharpe Ratio:  3.241
  Best Trade:    $4.87
  Worst Trade:   $-0.22
════════════════════════════════════════════════════════
```

Simulation includes:
- **0.2% slippage** — fills at slightly worse prices than displayed
- **1% fee** — applied to both legs combined
- **5-minute cooldown** — no repeat trades in the same market within 5 minutes
- **Capital tracking** — position sizes respect available balance

## Architecture

```
main.rs          CLI entry point (clap)
├── bot.rs       Orchestration — receives opportunities, manages balance/risk
│   ├── scanner/realtime.rs   WebSocket market scanner (tokio tasks)
│   │   └── api/websocket.rs  WsClient with zombie detection + reconnect
│   ├── executor.rs           EIP-712 signing + parallel order submission
│   ├── risk.rs               Position sizing + circuit breakers
│   └── db.rs                 SQLite persistence (sqlx)
├── api/
│   ├── gamma.rs    Gamma REST API (market discovery)
│   └── clob.rs     CLOB REST API (order book + order submission)
├── backtest.rs     Historical simulation engine
├── models.rs       Shared data types
├── config.rs       Environment-based configuration
└── error.rs        Typed error enum
```

### Key Design Choices

**Single-lock balance + circuit-breaker check**
The Python original used two nested async locks (`_balance_lock` inside `_execution_lock`), creating a TOCTOU window where another coroutine could modify the balance between the check and the deduction. The Rust version acquires a single `Mutex` guard that covers both the circuit-breaker check and the balance deduction atomically.

**`mpsc` channels instead of async callbacks**
The scanner sends `ArbitrageOpportunity` structs over a `tokio::sync::mpsc::unbounded_channel`. The bot receives from this channel in a simple `while let Some(opp) = rx.recv().await` loop — no callback registration, no shared function pointers, no lifetime gymnastics.

**Built-in zombie detection**
Each WebSocket receive loop uses `tokio::time::timeout(60s, stream.next())`. If no message arrives in 60 seconds, the function returns an error and the outer retry loop reconnects. No separate watchdog task needed.

**All-Decimal arithmetic**
`rust_decimal::Decimal` is used everywhere. The Python original mixed `Decimal` and `float` (e.g., `float(result.expected_profit)` for stats accumulation), which can silently lose precision on repeated additions.

## Security

- Private key is read from the environment and never written to disk or logged.
- EIP-712 digests are computed locally; no third-party signing service.
- HMAC-SHA256 signatures use the base64-decoded API secret (matching Polymarket's spec).
- `DRY_RUN=true` by default — you must explicitly opt into live trading.

## Development

```bash
# Run all tests
cargo test

# Check for issues without building
cargo check

# Build optimized binary
cargo build --release
# Binary at: target/release/rarb
```

## Audit

See [`AUDIT.md`](AUDIT.md) for a full accuracy and security review comparing this implementation to the Python original, including the bugs fixed and known limitations.

## License

MIT
