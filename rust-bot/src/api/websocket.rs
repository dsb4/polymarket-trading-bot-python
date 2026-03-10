/// WebSocket client for real-time Polymarket orderbook streaming.
///
/// Audit notes vs Python original:
/// - Zombie detection is built into the reconnect loop (stale_threshold check).
/// - Book state is stored in a DashMap for lock-free concurrent reads.
/// - Each WsClient runs in its own tokio task; price updates are sent over an mpsc channel.
use crate::{
    error::{BotError, Result},
    models::{OrderBook, PriceLevel, WsBookUpdate, WsPriceChange},
};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use parking_lot::RwLock;
use serde_json::json;
#[allow(unused_imports)]
use url::Url;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message};
use tracing::{debug, error, info, warn};

/// Maximum assets per WebSocket connection (Polymarket limit).
pub const MAX_ASSETS_PER_WS: usize = 500;

/// Message sent from WS worker to scanner.
#[derive(Debug, Clone)]
pub enum PriceUpdate {
    Book {
        asset_id: String,
        best_bid: Option<rust_decimal::Decimal>,
        best_ask: Option<rust_decimal::Decimal>,
        best_ask_size: Option<rust_decimal::Decimal>,
    },
    PriceChange {
        asset_id: String,
        side: String,
        price: rust_decimal::Decimal,
        size: rust_decimal::Decimal,
        best_bid: Option<rust_decimal::Decimal>,
        best_ask: Option<rust_decimal::Decimal>,
    },
}

struct WsState {
    /// Cached orderbooks (token_id → OrderBook)
    books: HashMap<String, OrderBook>,
    /// Last message timestamp (Unix ms)
    last_msg_at: i64,
}

pub struct WsClient {
    ws_url: String,
    state: Arc<RwLock<WsState>>,
    last_msg_ts: Arc<AtomicI64>,
    tx: tokio::sync::mpsc::UnboundedSender<PriceUpdate>,
    subscribed: Vec<String>,
}

impl WsClient {
    pub fn new(
        ws_url: String,
        tx: tokio::sync::mpsc::UnboundedSender<PriceUpdate>,
    ) -> Self {
        let now = Utc::now().timestamp_millis();
        Self {
            ws_url,
            state: Arc::new(RwLock::new(WsState {
                books: HashMap::new(),
                last_msg_at: now,
            })),
            last_msg_ts: Arc::new(AtomicI64::new(now)),
            tx,
            subscribed: Vec::new(),
        }
    }

    /// Seconds since the last message was received (zombie detection).
    pub fn seconds_since_last_message(&self) -> f64 {
        let now = Utc::now().timestamp_millis();
        let last = self.last_msg_ts.load(Ordering::Relaxed);
        (now - last) as f64 / 1000.0
    }

    /// Subscribe to a batch of token IDs and run the receive loop.
    /// On disconnect, reconnects with exponential backoff.
    pub async fn run(&mut self, token_ids: Vec<String>) -> Result<()> {
        self.subscribed = token_ids;
        let mut delay_secs = 1u64;

        loop {
            match self.connect_and_listen().await {
                Ok(()) => {
                    info!("WebSocket disconnected cleanly");
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                }
            }

            warn!(
                "Reconnecting WebSocket in {}s (subscribed to {} tokens)",
                delay_secs,
                self.subscribed.len()
            );
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            delay_secs = (delay_secs * 2).min(60);
        }
    }

    async fn connect_and_listen(&mut self) -> Result<()> {
        let (ws_stream, _response) = timeout(
            Duration::from_secs(15),
            connect_async_tls_with_config(self.ws_url.as_str(), None, false, None),
        )
        .await
        .map_err(|_| BotError::WebSocket("Connection timed out".into()))?
        .map_err(|e| BotError::WebSocket(e.to_string()))?;

        let (mut sink, mut stream) = ws_stream.split();

        // Subscribe to all tokens in batches of 200 to avoid huge messages
        for chunk in self.subscribed.chunks(200) {
            let msg = json!({
                "assets_ids": chunk,
                "type": "market",
            });
            sink.send(Message::Text(msg.to_string()))
                .await
                .map_err(|e| BotError::WebSocket(e.to_string()))?;
        }

        info!("WebSocket connected, subscribed to {} tokens", self.subscribed.len());

        let state = Arc::clone(&self.state);
        let ts_atomic = Arc::clone(&self.last_msg_ts);
        let tx = self.tx.clone();

        // Stale/zombie detection: if no message in 60s, bail and reconnect
        const STALE_SECS: u64 = 60;

        loop {
            match timeout(Duration::from_secs(STALE_SECS), stream.next()).await {
                Err(_elapsed) => {
                    warn!("WebSocket stale for {}s, forcing reconnect", STALE_SECS);
                    return Err(BotError::WebSocket("Stale connection".into()));
                }
                Ok(None) => {
                    debug!("WebSocket stream ended");
                    return Ok(());
                }
                Ok(Some(Err(e))) => {
                    return Err(BotError::WebSocket(e.to_string()));
                }
                Ok(Some(Ok(msg))) => {
                    let now_ms = Utc::now().timestamp_millis();
                    ts_atomic.store(now_ms, Ordering::Relaxed);

                    match msg {
                        Message::Text(text) => {
                            Self::handle_message(&text, &state, &tx);
                        }
                        Message::Ping(payload) => {
                            sink.send(Message::Pong(payload))
                                .await
                                .map_err(|e| BotError::WebSocket(e.to_string()))?;
                        }
                        Message::Close(_) => {
                            info!("WebSocket received Close frame");
                            return Ok(());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn handle_message(
        text: &str,
        state: &Arc<RwLock<WsState>>,
        tx: &tokio::sync::mpsc::UnboundedSender<PriceUpdate>,
    ) {
        // Messages can be arrays or single objects
        let values: Vec<serde_json::Value> = if text.starts_with('[') {
            serde_json::from_str(text).unwrap_or_default()
        } else {
            match serde_json::from_str::<serde_json::Value>(text) {
                Ok(v) => vec![v],
                Err(_) => return,
            }
        };

        for value in values {
            let event_type = value
                .get("event_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            match event_type {
                "book" => {
                    if let Ok(update) = serde_json::from_value::<WsBookUpdate>(value) {
                        Self::handle_book(update, state, tx);
                    }
                }
                "price_change" => {
                    if let Ok(change) = serde_json::from_value::<WsPriceChange>(value) {
                        Self::handle_price_change(change, state, tx);
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_book(
        update: WsBookUpdate,
        state: &Arc<RwLock<WsState>>,
        tx: &tokio::sync::mpsc::UnboundedSender<PriceUpdate>,
    ) {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let parse_levels = |levels: &[crate::models::WsPriceLevel]| -> Vec<PriceLevel> {
            levels
                .iter()
                .filter_map(|l| {
                    let price = Decimal::from_str(&l.price).ok()?;
                    let size = Decimal::from_str(&l.size).ok()?;
                    Some(PriceLevel { price, size })
                })
                .collect()
        };

        let ob = OrderBook {
            asset_id: update.asset_id.clone(),
            bids: parse_levels(&update.bids),
            asks: parse_levels(&update.asks),
        };

        let best_ask_size = ob.best_ask_size();
        let best_ask = ob.best_ask();
        let best_bid = ob.best_bid();

        // Update cached book
        {
            let mut s = state.write();
            s.books.insert(update.asset_id.clone(), ob);
        }

        let _ = tx.send(PriceUpdate::Book {
            asset_id: update.asset_id,
            best_bid,
            best_ask,
            best_ask_size,
        });
    }

    fn handle_price_change(
        change: WsPriceChange,
        state: &Arc<RwLock<WsState>>,
        tx: &tokio::sync::mpsc::UnboundedSender<PriceUpdate>,
    ) {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let price = match Decimal::from_str(&change.price) {
            Ok(p) => p,
            Err(_) => return,
        };
        let size = match Decimal::from_str(&change.size) {
            Ok(s) => s,
            Err(_) => return,
        };
        let best_bid = change.best_bid.as_deref().and_then(|s| Decimal::from_str(s).ok());
        let best_ask = change.best_ask.as_deref().and_then(|s| Decimal::from_str(s).ok());

        // Update cached book based on change
        {
            let mut s = state.write();
            if let Some(book) = s.books.get_mut(&change.asset_id) {
                match change.side.as_str() {
                    "SELL" | "sell" => {
                        // Update or remove ask level
                        if size.is_zero() {
                            book.asks.retain(|l| l.price != price);
                        } else {
                            if let Some(lvl) = book.asks.iter_mut().find(|l| l.price == price) {
                                lvl.size = size;
                            } else {
                                book.asks.push(PriceLevel { price, size });
                                book.asks.sort_by(|a, b| a.price.cmp(&b.price));
                            }
                        }
                    }
                    "BUY" | "buy" => {
                        if size.is_zero() {
                            book.bids.retain(|l| l.price != price);
                        } else {
                            if let Some(lvl) = book.bids.iter_mut().find(|l| l.price == price) {
                                lvl.size = size;
                            } else {
                                book.bids.push(PriceLevel { price, size });
                                book.bids.sort_by(|a, b| b.price.cmp(&a.price));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        let _ = tx.send(PriceUpdate::PriceChange {
            asset_id: change.asset_id,
            side: change.side,
            price,
            size,
            best_bid,
            best_ask,
        });
    }

    /// Get a cached orderbook snapshot.
    pub fn get_orderbook(&self, token_id: &str) -> Option<OrderBook> {
        self.state.read().books.get(token_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_orderbook_best_ask() {
        let ob = OrderBook {
            asset_id: "tok1".into(),
            bids: vec![],
            asks: vec![
                PriceLevel { price: dec!(0.50), size: dec!(100) },
                PriceLevel { price: dec!(0.49), size: dec!(200) },
                PriceLevel { price: dec!(0.51), size: dec!(50) },
            ],
        };
        assert_eq!(ob.best_ask(), Some(dec!(0.49)));
        assert_eq!(ob.best_ask_size(), Some(dec!(200)));
    }

    #[test]
    fn test_depth_at_ask() {
        let ob = OrderBook {
            asset_id: "tok1".into(),
            bids: vec![],
            asks: vec![
                PriceLevel { price: dec!(0.49), size: dec!(200) },
                PriceLevel { price: dec!(0.491), size: dec!(100) },
                PriceLevel { price: dec!(0.50), size: dec!(300) },
            ],
        };
        // Within 1% of 0.49 → prices up to 0.4949 → only 0.49 and 0.491 qualify
        let depth = ob.depth_at_ask(dec!(0.01));
        assert_eq!(depth, dec!(300));
    }
}
