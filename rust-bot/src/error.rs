use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("API error: {0}")]
    Api(String),

    #[error("HTTP error: {status} - {body}")]
    Http { status: u16, body: String },

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("Signing error: {0}")]
    Signing(String),

    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Insufficient balance: need ${required:.2}, have ${available:.2}")]
    InsufficientBalance { required: f64, available: f64 },

    #[error("Order execution failed: {reason}")]
    ExecutionFailed { reason: String },

    #[error("Market not found: {market_id}")]
    MarketNotFound { market_id: String },

    #[error("No arbitrage opportunity: combined cost {combined:.4} >= threshold")]
    NoArbitrage { combined: f64 },

    #[error("Risk check failed: {reason}")]
    RiskCheckFailed { reason: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Request error: {0}")]
    Request(#[from] reqwest::Error),

    #[error("URL parse error: {0}")]
    Url(#[from] url::ParseError),

    #[error("Hex decode error: {0}")]
    HexDecode(#[from] hex::FromHexError),
}

pub type Result<T> = std::result::Result<T, BotError>;
