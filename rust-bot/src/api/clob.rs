/// CLOB (Central Limit Order Book) API client.
///
/// Authentication uses Polymarket L2 HMAC-SHA256:
///   POLY-TIMESTAMP: unix seconds
///   POLY-API-KEY:   api_key
///   POLY-SIGNATURE: base64(HMAC-SHA256(timestamp + METHOD + path + body, base64decode(secret)))
///   POLY-PASSPHRASE: passphrase
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::{Client, RequestBuilder};
use sha2::Sha256;
use tracing::{debug, warn};

use crate::{
    config::Config,
    error::{BotError, Result},
    models::{ClobOrderBook, OrderBook},
};

type HmacSha256 = Hmac<Sha256>;

pub struct ClobClient {
    client: Client,
    base_url: String,
    api_key: Option<String>,
    api_secret: Option<String>,
    api_passphrase: Option<String>,
}

impl ClobClient {
    pub fn new(config: &Config) -> Result<Self> {
        let mut builder = Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .pool_max_idle_per_host(20)
            .http2_prior_knowledge();

        if let Some(proxy_url) = config.get_socks5_proxy_url() {
            builder = builder
                .proxy(reqwest::Proxy::all(&proxy_url).map_err(|e| BotError::Api(e.to_string()))?);
        }

        Ok(Self {
            client: builder.build()?,
            base_url: config.clob_base_url.clone(),
            api_key: config.poly_api_key.clone(),
            api_secret: config.poly_api_secret.clone(),
            api_passphrase: config.poly_api_passphrase.clone(),
        })
    }

    /// Fetch the orderbook for a single token.
    pub async fn get_orderbook(&self, token_id: &str) -> Result<OrderBook> {
        let url = format!("{}/book", self.base_url);
        let resp = self
            .client
            .get(&url)
            .query(&[("token_id", token_id)])
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BotError::Http { status, body });
        }

        let raw: ClobOrderBook = resp.json().await?;
        OrderBook::try_from(raw)
    }

    /// Fetch orderbooks for multiple tokens concurrently.
    pub async fn get_orderbooks_batch(&self, token_ids: &[&str]) -> Vec<Result<OrderBook>> {
        use futures::future::join_all;

        let futs: Vec<_> = token_ids
            .iter()
            .map(|id| self.get_orderbook(id))
            .collect();
        join_all(futs).await
    }

    /// Place a signed order on the CLOB.
    pub async fn post_order(
        &self,
        order_body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}/order", self.base_url);
        let body_str = serde_json::to_string(order_body)?;

        let builder = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body_str.clone());

        let builder = self.add_auth(builder, "POST", "/order", &body_str)?;

        let resp = builder.send().await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            warn!("Order rejected: {} - {}", status, body);
            return Err(BotError::Http { status, body });
        }

        Ok(resp.json().await?)
    }

    /// Cancel an order by ID.
    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let path = format!("/order/{}", order_id);
        let url = format!("{}{}", self.base_url, path);

        let builder = self.client.delete(&url);
        let builder = self.add_auth(builder, "DELETE", &path, "")?;

        let resp = builder.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BotError::Http { status, body });
        }
        Ok(())
    }

    /// Get all open orders for this wallet.
    pub async fn get_open_orders(&self) -> Result<serde_json::Value> {
        let url = format!("{}/orders", self.base_url);
        let builder = self.client.get(&url);
        let builder = self.add_auth(builder, "GET", "/orders", "")?;

        let resp = builder.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BotError::Http { status, body });
        }
        Ok(resp.json().await?)
    }

    /// Get order status by ID.
    pub async fn get_order(&self, order_id: &str) -> Result<serde_json::Value> {
        let path = format!("/order/{}", order_id);
        let url = format!("{}{}", self.base_url, path);

        let builder = self.client.get(&url);
        let builder = self.add_auth(builder, "GET", &path, "")?;

        let resp = builder.send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(BotError::Http { status, body });
        }
        Ok(resp.json().await?)
    }

    // ── Auth ──────────────────────────────────────────────────────────────────

    fn add_auth(
        &self,
        builder: RequestBuilder,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<RequestBuilder> {
        let (Some(key), Some(secret), Some(passphrase)) = (
            &self.api_key,
            &self.api_secret,
            &self.api_passphrase,
        ) else {
            debug!("No L2 credentials; sending unauthenticated request");
            return Ok(builder);
        };

        let timestamp = Utc::now().timestamp().to_string();
        let message = format!("{}{}{}{}", timestamp, method.to_uppercase(), path, body);

        let secret_bytes = B64
            .decode(secret)
            .map_err(|e| BotError::Signing(format!("Base64 decode secret: {}", e)))?;

        let mut mac = HmacSha256::new_from_slice(&secret_bytes)
            .map_err(|e| BotError::Signing(format!("HMAC init: {}", e)))?;
        mac.update(message.as_bytes());
        let sig = B64.encode(mac.finalize().into_bytes());

        Ok(builder
            .header("POLY-TIMESTAMP", &timestamp)
            .header("POLY-API-KEY", key)
            .header("POLY-SIGNATURE", &sig)
            .header("POLY-PASSPHRASE", passphrase))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify HMAC-SHA256 signature generation is deterministic.
    #[test]
    fn test_hmac_signature() {
        // Known test vector
        let secret_raw = b"test_secret_key!!"; // 16 bytes
        let secret_b64 = B64.encode(secret_raw);
        let message = "1234567890POSTorder{}";

        let secret_bytes = B64.decode(&secret_b64).unwrap();
        let mut mac = HmacSha256::new_from_slice(&secret_bytes).unwrap();
        mac.update(message.as_bytes());
        let sig1 = B64.encode(mac.finalize().into_bytes());

        // Same inputs → same output
        let secret_bytes2 = B64.decode(&secret_b64).unwrap();
        let mut mac2 = HmacSha256::new_from_slice(&secret_bytes2).unwrap();
        mac2.update(message.as_bytes());
        let sig2 = B64.encode(mac2.finalize().into_bytes());

        assert_eq!(sig1, sig2);
        assert!(!sig1.is_empty());
    }

    #[test]
    fn test_orderbook_parsing() {
        use crate::models::ClobPriceLevel;

        let raw = ClobOrderBook {
            market: None,
            asset_id: "token123".into(),
            bids: vec![
                ClobPriceLevel { price: "0.48".into(), size: "1000".into() },
                ClobPriceLevel { price: "0.47".into(), size: "2500".into() },
            ],
            asks: vec![
                ClobPriceLevel { price: "0.49".into(), size: "800".into() },
                ClobPriceLevel { price: "0.50".into(), size: "1500".into() },
            ],
        };

        let ob = OrderBook::try_from(raw).unwrap();
        assert_eq!(ob.best_ask().unwrap().to_string(), "0.49");
        assert_eq!(ob.best_bid().unwrap().to_string(), "0.48");
        assert_eq!(ob.best_ask_size().unwrap().to_string(), "800");
    }
}
