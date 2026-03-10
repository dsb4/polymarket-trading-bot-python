/// Order executor with native EIP-712 signing.
///
/// Audit improvements vs Python original:
/// - EIP-712 implemented natively with sha3 + k256 (no external signing lib required).
/// - Parallel YES/NO order submission with tokio::join!.
/// - Timeout enforcement via tokio::time::timeout.
/// - Dry run mode is enforced at this level (not relying on upstream config reads).
use crate::{
    api::clob::ClobClient,
    config::Config,
    error::{BotError, Result},
    models::{ArbitrageOpportunity, ExecutionResult, ExecutionStatus, Order, OrderSide},
};
use chrono::Utc;
use k256::{
    ecdsa::{signature::SignerMut, Signature, SigningKey},
    SecretKey,
};
use rand::Rng;
use rust_decimal::Decimal;
use sha3::{Digest, Keccak256};
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;
use tracing::{info, warn};

// ── Polymarket contract addresses ─────────────────────────────────────────────

const CTF_EXCHANGE_ADDR: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const NEG_RISK_EXCHANGE_ADDR: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

// EIP-712 type strings
const DOMAIN_TYPE_HASH_INPUT: &str =
    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const ORDER_TYPE_HASH_INPUT: &str =
    "Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,\
     uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,\
     uint256 feeRateBps,uint8 side,uint8 signatureType)";

// ── EIP-712 helpers ───────────────────────────────────────────────────────────

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(data);
    h.finalize().into()
}

/// ABI-encode a `uint256` value (big-endian, 32 bytes).
fn abi_uint256(val: u128) -> [u8; 32] {
    let mut buf = [0u8; 32];
    let bytes = val.to_be_bytes();
    buf[16..].copy_from_slice(&bytes);
    buf
}

/// ABI-encode a `uint256` from a U256-like 32-byte big-endian array.
fn abi_uint256_bytes(val: [u8; 32]) -> [u8; 32] {
    val
}

/// ABI-encode an Ethereum address (20 bytes → zero-padded to 32 bytes).
fn abi_address(addr_hex: &str) -> [u8; 32] {
    let clean = addr_hex.trim_start_matches("0x");
    let bytes = hex::decode(clean).unwrap_or_default();
    let mut buf = [0u8; 32];
    let start = 32 - bytes.len().min(20);
    buf[start..].copy_from_slice(&bytes[..bytes.len().min(20)]);
    buf
}

/// ABI-encode a `uint8` (zero-padded to 32 bytes).
fn abi_uint8(val: u8) -> [u8; 32] {
    let mut buf = [0u8; 32];
    buf[31] = val;
    buf
}

/// Compute the EIP-712 domain separator for Polymarket.
fn compute_domain_separator(chain_id: u64, contract_address: &str) -> [u8; 32] {
    let type_hash = keccak256(DOMAIN_TYPE_HASH_INPUT.as_bytes());
    let name_hash = keccak256(b"Polymarket CTF Exchange");
    let version_hash = keccak256(b"1");

    let mut chain_id_bytes = [0u8; 32];
    chain_id_bytes[24..].copy_from_slice(&chain_id.to_be_bytes());

    let mut encoded = Vec::with_capacity(32 * 5);
    encoded.extend_from_slice(&type_hash);
    encoded.extend_from_slice(&name_hash);
    encoded.extend_from_slice(&version_hash);
    encoded.extend_from_slice(&chain_id_bytes);
    encoded.extend_from_slice(&abi_address(contract_address));

    keccak256(&encoded)
}

/// Compute EIP-712 digest for a Polymarket order.
pub fn compute_order_digest(order: &Order, chain_id: u64, neg_risk: bool) -> [u8; 32] {
    let contract = if neg_risk {
        NEG_RISK_EXCHANGE_ADDR
    } else {
        CTF_EXCHANGE_ADDR
    };

    let type_hash = keccak256(ORDER_TYPE_HASH_INPUT.as_bytes());
    let domain_separator = compute_domain_separator(chain_id, contract);

    // Token ID as uint256 (it's a large number stored as a decimal string)
    let token_id_num: u128 = order.token_id.parse().unwrap_or(0);

    let mut struct_bytes = Vec::with_capacity(32 * 12);
    struct_bytes.extend_from_slice(&type_hash);
    struct_bytes.extend_from_slice(&abi_uint256(order.salt as u128));
    struct_bytes.extend_from_slice(&abi_address(&order.maker));
    struct_bytes.extend_from_slice(&abi_address(&order.signer));
    struct_bytes.extend_from_slice(&abi_address(&order.taker));
    struct_bytes.extend_from_slice(&abi_uint256(token_id_num));
    struct_bytes.extend_from_slice(&abi_uint256(order.maker_amount));
    struct_bytes.extend_from_slice(&abi_uint256(order.taker_amount));
    struct_bytes.extend_from_slice(&abi_uint256(order.expiration as u128));
    struct_bytes.extend_from_slice(&abi_uint256(order.nonce as u128));
    struct_bytes.extend_from_slice(&abi_uint256(order.fee_rate_bps as u128));
    struct_bytes.extend_from_slice(&abi_uint8(order.side.clone().into()));
    struct_bytes.extend_from_slice(&abi_uint8(order.signature_type));

    let struct_hash = keccak256(&struct_bytes);

    // Final digest: "\x19\x01" || domainSeparator || structHash
    let mut digest_bytes = Vec::with_capacity(2 + 32 + 32);
    digest_bytes.extend_from_slice(&[0x19, 0x01]);
    digest_bytes.extend_from_slice(&domain_separator);
    digest_bytes.extend_from_slice(&struct_hash);

    keccak256(&digest_bytes)
}

/// Sign a digest with a secp256k1 private key. Returns `0x...` hex signature.
pub fn sign_digest(digest: &[u8; 32], private_key_hex: &str) -> Result<String> {
    let key_bytes = hex::decode(private_key_hex.trim_start_matches("0x"))
        .map_err(|e| BotError::Signing(e.to_string()))?;

    let secret = SecretKey::from_slice(&key_bytes)
        .map_err(|e| BotError::Signing(e.to_string()))?;
    let mut signing_key = SigningKey::from(secret);

    // Sign the raw digest (Ethereum expects the raw 32-byte hash)
    let (sig, recovery_id): (Signature, _) = signing_key
        .sign_digest_recoverable(sha3::Keccak256::new_with_prefix(digest))
        .map_err(|e| BotError::Signing(e.to_string()))?;

    // Encode as r || s || v (65 bytes)
    let sig_bytes = sig.to_bytes();
    let v = recovery_id.to_byte() + 27; // Ethereum v convention
    let mut full_sig = Vec::with_capacity(65);
    full_sig.extend_from_slice(&sig_bytes);
    full_sig.push(v);

    Ok(format!("0x{}", hex::encode(full_sig)))
}

// ── Price conversion helpers ──────────────────────────────────────────────────

/// Convert a Decimal price to Polymarket's USDC integer representation.
/// Polymarket uses 6 decimal places (1 USDC = 1_000_000).
fn price_to_usdc(price: Decimal, shares: Decimal) -> u128 {
    let usdc = price * shares * Decimal::from(1_000_000u64);
    // Round to integer and convert to u128
    let rounded = usdc.round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);
    u128::try_from(rounded).unwrap_or(0)
}

/// Convert shares to Polymarket's outcome token integer representation.
/// Outcome tokens also use 6 decimal places.
fn shares_to_tokens(shares: Decimal) -> u128 {
    let tokens = shares * Decimal::from(1_000_000u64);
    let rounded = tokens.round_dp_with_strategy(0, rust_decimal::RoundingStrategy::ToZero);
    u128::try_from(rounded).unwrap_or(0)
}

// ── OrderExecutor ─────────────────────────────────────────────────────────────

pub struct OrderExecutor {
    clob: Arc<ClobClient>,
    config: Arc<Config>,
    dry_run: bool,
}

impl OrderExecutor {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let clob = Arc::new(ClobClient::new(&config)?);
        let dry_run = config.dry_run;
        Ok(Self { clob, config, dry_run })
    }

    pub async fn execute(
        &self,
        opportunity: &ArbitrageOpportunity,
        detection_ts_ms: Option<u64>,
    ) -> Result<ExecutionResult> {
        let exec_start = std::time::Instant::now();

        if self.dry_run {
            return Ok(self.simulate_execution(opportunity, exec_start, detection_ts_ms));
        }

        let private_key = self
            .config
            .private_key
            .as_deref()
            .ok_or_else(|| BotError::Config("PRIVATE_KEY not set".into()))?;
        let wallet = self
            .config
            .wallet_address
            .as_deref()
            .ok_or_else(|| BotError::Config("WALLET_ADDRESS not set".into()))?;

        let trade_size = opportunity.max_trade_size;
        let neg_risk = opportunity.market.neg_risk;

        // Build both orders
        let yes_order = self.build_order(
            wallet,
            &opportunity.market.yes_token.token_id,
            opportunity.yes_ask,
            trade_size,
        );
        let no_order = self.build_order(
            wallet,
            &opportunity.market.no_token.token_id,
            opportunity.no_ask,
            trade_size,
        );

        // Sign both orders
        let yes_digest = compute_order_digest(&yes_order, self.config.chain_id, neg_risk);
        let no_digest = compute_order_digest(&no_order, self.config.chain_id, neg_risk);

        let yes_sig = sign_digest(&yes_digest, private_key)?;
        let no_sig = sign_digest(&no_digest, private_key)?;

        let yes_body = self.order_to_json(&yes_order, &yes_sig, wallet);
        let no_body = self.order_to_json(&no_order, &no_sig, wallet);

        // Submit both orders in parallel with 10s timeout
        let clob = Arc::clone(&self.clob);
        let clob2 = Arc::clone(&self.clob);

        let (yes_result, no_result) = tokio::join!(
            timeout(Duration::from_secs(10), clob.post_order(&yes_body)),
            timeout(Duration::from_secs(10), clob2.post_order(&no_body)),
        );

        let latency_ms = exec_start.elapsed().as_millis() as u64;
        let end_to_end_ms = detection_ts_ms.map(|start| {
            Utc::now().timestamp_millis() as u64 - start
        });

        let yes_id = match yes_result {
            Ok(Ok(v)) => {
                let id = v["orderID"].as_str().map(String::from)
                    .or_else(|| v["id"].as_str().map(String::from));
                info!("YES order submitted: {:?} ({}ms)", id, latency_ms);
                id
            }
            Ok(Err(e)) => {
                warn!("YES order failed: {}", e);
                None
            }
            Err(_) => {
                warn!("YES order timed out after 10s");
                None
            }
        };

        let no_id = match no_result {
            Ok(Ok(v)) => {
                let id = v["orderID"].as_str().map(String::from)
                    .or_else(|| v["id"].as_str().map(String::from));
                info!("NO order submitted: {:?} ({}ms)", id, latency_ms);
                id
            }
            Ok(Err(e)) => {
                warn!("NO order failed: {}", e);
                None
            }
            Err(_) => {
                warn!("NO order timed out after 10s");
                None
            }
        };

        let status = match (&yes_id, &no_id) {
            (Some(_), Some(_)) => ExecutionStatus::Filled,
            (None, None) => ExecutionStatus::Failed,
            _ => ExecutionStatus::PartialFill,
        };

        let expected_profit = opportunity.max_trade_size * opportunity.profit_pct;
        let actual_cost = opportunity.max_trade_size * opportunity.combined_cost;

        if end_to_end_ms.is_some() {
            info!(
                "End-to-end latency: {}ms",
                end_to_end_ms.unwrap_or(latency_ms)
            );
        }

        Ok(ExecutionResult {
            status,
            yes_order_id: yes_id,
            no_order_id: no_id,
            expected_profit,
            actual_cost,
            executed_at: Utc::now(),
            latency_ms: Some(latency_ms),
        })
    }

    fn simulate_execution(
        &self,
        opportunity: &ArbitrageOpportunity,
        start: std::time::Instant,
        _detection_ts_ms: Option<u64>,
    ) -> ExecutionResult {
        let trade_size = opportunity.max_trade_size;
        let expected_profit = trade_size * opportunity.profit_pct;
        let actual_cost = trade_size * opportunity.combined_cost;
        let latency_ms = start.elapsed().as_millis() as u64;

        info!(
            "[DRY RUN] Would trade {} shares in {} | profit=${:.2} combined={:.4}",
            trade_size,
            &opportunity.market.question[..opportunity.market.question.len().min(40)],
            expected_profit,
            opportunity.combined_cost,
        );

        ExecutionResult {
            status: ExecutionStatus::DryRun,
            yes_order_id: Some("dry-run-yes".into()),
            no_order_id: Some("dry-run-no".into()),
            expected_profit,
            actual_cost,
            executed_at: Utc::now(),
            latency_ms: Some(latency_ms),
        }
    }

    fn build_order(
        &self,
        wallet: &str,
        token_id: &str,
        price: Decimal,
        shares: Decimal,
    ) -> Order {
        let salt: u64 = rand::thread_rng().gen();
        let maker_amount = price_to_usdc(price, shares);
        let taker_amount = shares_to_tokens(shares);

        Order {
            salt,
            maker: wallet.to_string(),
            signer: wallet.to_string(),
            taker: "0x0000000000000000000000000000000000000000".into(),
            token_id: token_id.to_string(),
            maker_amount,
            taker_amount,
            expiration: 0,
            nonce: 0,
            fee_rate_bps: 0,
            side: OrderSide::Buy,
            signature_type: 0, // EOA
            signature: None,
        }
    }

    fn order_to_json(&self, order: &Order, sig: &str, owner: &str) -> serde_json::Value {
        serde_json::json!({
            "order": {
                "salt": order.salt.to_string(),
                "maker": order.maker,
                "signer": order.signer,
                "taker": order.taker,
                "tokenId": order.token_id,
                "makerAmount": order.maker_amount.to_string(),
                "takerAmount": order.taker_amount.to_string(),
                "expiration": order.expiration.to_string(),
                "nonce": order.nonce.to_string(),
                "feeRateBps": order.fee_rate_bps.to_string(),
                "side": "BUY",
                "signatureType": order.signature_type,
            },
            "signature": sig,
            "owner": owner,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn test_price_to_usdc() {
        // $0.48 for 100 shares = $48 = 48_000_000 units
        assert_eq!(price_to_usdc(dec!(0.48), dec!(100)), 48_000_000);
    }

    #[test]
    fn test_abi_uint256() {
        let enc = abi_uint256(1_000_000);
        assert_eq!(&enc[28..], &1_000_000u32.to_be_bytes());
    }

    #[test]
    fn test_abi_address() {
        let addr = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
        let enc = abi_address(addr);
        // First 12 bytes should be zero (left-padded)
        assert_eq!(&enc[..12], &[0u8; 12]);
    }

    #[test]
    fn test_domain_separator_deterministic() {
        let ds1 = compute_domain_separator(137, CTF_EXCHANGE_ADDR);
        let ds2 = compute_domain_separator(137, CTF_EXCHANGE_ADDR);
        assert_eq!(ds1, ds2);

        // Different chain ID → different separator
        let ds_other = compute_domain_separator(1, CTF_EXCHANGE_ADDR);
        assert_ne!(ds1, ds_other);
    }

    #[test]
    fn test_compute_order_digest_deterministic() {
        let order = Order {
            salt: 12345,
            maker: "0xabcdef1234567890abcdef1234567890abcdef12".into(),
            signer: "0xabcdef1234567890abcdef1234567890abcdef12".into(),
            taker: "0x0000000000000000000000000000000000000000".into(),
            token_id: "99999".into(),
            maker_amount: 480_000,
            taker_amount: 1_000_000,
            expiration: 0,
            nonce: 0,
            fee_rate_bps: 0,
            side: OrderSide::Buy,
            signature_type: 0,
            signature: None,
        };

        let digest1 = compute_order_digest(&order, 137, false);
        let digest2 = compute_order_digest(&order, 137, false);
        assert_eq!(digest1, digest2);

        // neg_risk → different contract → different digest
        let digest_neg = compute_order_digest(&order, 137, true);
        assert_ne!(digest1, digest_neg);
    }
}
