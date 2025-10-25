//! Signet bundle delivery implementation.
//!
//! This module implements transaction delivery for Signet L2 using bundle submission.
//! Bundles combine L1 host transactions with L2 transactions and are submitted to
//! the Signet transaction cache.
//!
//! ## Current Limitations
//!
//! This initial implementation focuses on bundle creation and submission infrastructure.
//! Full integration with Phase 1 discovered SignedOrders (correlating intents with
//! fulfillment transactions) requires additional work in the solver core to pass
//! the necessary context through the delivery pipeline.

use crate::{DeliveryError, DeliveryInterface};
use alloy_eips::eip2718::Encodable2718;
use alloy_network::EthereumWallet;
use alloy_primitives::Bytes;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types::mev::EthSendBundle;
use alloy_signer_local::PrivateKeySigner;
use async_trait::async_trait;
use signet_bundle::SignetEthBundle;
use signet_tx_cache::client::TxCache;
use signet_types::SignedFill;
use solver_types::{
	ConfigSchema, Field, FieldType, NetworksConfig, Schema, Transaction as SolverTransaction,
	TransactionHash, TransactionReceipt,
};
use std::sync::Arc;
use tokio::time::{sleep, Duration};

const DEFAULT_BLOCK_NUMBER: u64 = 1;
const NUM_TARGET_BLOCKS: u64 = 10;
/// Default gas limit for transactions.
const DEFAULT_GAS_LIMIT: u64 = 1_000_000;
/// Default priority fee multiplier for transactions.
const DEFAULT_PRIORITY_FEE_MULTIPLIER: u64 = 16;
/// Multiplier for converting gwei to wei.
const GWEI_TO_WEI: u64 = 1_000_000_000;

/// Signet bundle delivery implementation configuration.
#[derive(Debug, Clone)]
pub struct SignetBundleConfig {
	/// Signet chain name (e.g., "pecorino")
	pub chain_name: String,
	/// Target block number for bundles (optional, defaults to 1)
	pub target_block: Option<u64>,
	/// Rollup chain ID (L2)
	pub rollup_chain_id: u64,
	/// Host chain ID (L1) where fills are executed
	pub host_chain_id: u64,
	/// OrderOrigin contract address on L2
	pub order_origin_address: alloy_primitives::Address,
	/// OrderDestination contract address on L1
	pub order_destination_address: alloy_primitives::Address,
	/// Address where filler receives input tokens
	pub filler_recipient: alloy_primitives::Address,
}

/// Signet bundle delivery implementation.
///
/// Submits transactions to Signet L2 by wrapping them in bundles and sending
/// to the transaction cache.
pub struct SignetBundleDelivery {
	/// Delivery configuration
	config: SignetBundleConfig,
	/// Networks configuration
	networks: NetworksConfig,
	/// Signet cache client
	cache_client: Arc<TxCache>,
	/// Solver's signer for creating SignedFills
	#[allow(dead_code)] // Used in TODO: create_signed_fill implementation
	signer: PrivateKeySigner,
	/// Simple flag to track if we've tried fetching block numbers via RPC
	/// (no need to store complex provider types, just call RPC directly when needed)
	_rpc_enabled: bool,
}

impl SignetBundleDelivery {
	/// Creates a new Signet bundle delivery instance.
	pub fn new(
		config: SignetBundleConfig,
		networks: NetworksConfig,
		signer: PrivateKeySigner,
	) -> Result<Self, DeliveryError> {
		// Validate chain name
		if config.chain_name.is_empty() {
			return Err(DeliveryError::Network(
				"chain_name cannot be empty".to_string(),
			));
		}

		// Build cache client based on chain name
		let cache_client = if config.chain_name == "pecorino" {
			TxCache::pecorino()
		} else {
			// Construct URL for other chains
			let url = format!("https://cache.{}.signet.sh", config.chain_name);
			TxCache::new_from_string(&url).map_err(|e| {
				DeliveryError::Network(format!("Failed to create Signet cache client: {}", e))
			})?
		};

		Ok(Self {
			config,
			networks,
			cache_client: Arc::new(cache_client),
			signer,
			_rpc_enabled: true,
		})
	}

	/// Creates a series of bundles for subsequent blocks from a solver fill transaction.
	///
	/// Generates NUM_TARGET_BLOCKS bundles, each targeting a block from
	/// (current_block + 1) up to (current_block + NUM_TARGET_BLOCKS).
	async fn create_bundles(
		&self,
		tx: &SolverTransaction,
	) -> Result<Vec<SignetEthBundle>, DeliveryError> {
		// --- (1) SignedOrder 및 L2 Initiate Tx 생성 로직은 하나만 수행

		// Extract SignedOrder from transaction metadata
		let signed_order = if let Some(metadata) = &tx.metadata {
			// ... (SignedOrder Deserialization 로직 유지)
			tracing::debug!("Deserializing SignedOrder from transaction metadata");
			let signed_order = serde_json::from_value::<signet_types::SignedOrder>(
				metadata.clone(),
			)
			.map_err(|e| {
				DeliveryError::Network(format!(
					"Failed to deserialize SignedOrder from metadata: {}",
					e
				))
			})?;
			tracing::info!(
				outputs_count = signed_order.outputs.len(),
				"Successfully deserialized SignedOrder"
			);
			signed_order
		} else {
			return Err(DeliveryError::Network(
				"No SignedOrder metadata in transaction for Signet bundle".to_string(),
			));
		};

		// Get current rollup block number
		let current_block = self.get_block_number(self.config.rollup_chain_id).await?;

		// Create SignedFills for all target chains (both rollup and host)
		let signed_fills = self.create_signed_fills(&signed_order).await?;

		// Build rollup transaction requests (collect all requests first, then sign together)
		let mut rollup_tx_requests = Vec::new();

		// First, add fill transaction for rollup chain if it exists
		if let Some(rollup_fill) = signed_fills.get(&self.config.rollup_chain_id) {
			let fill_tx_request = rollup_fill.to_fill_tx(self.config.order_origin_address);
			rollup_tx_requests.push(fill_tx_request);
			tracing::debug!("Added rollup fill transaction request");
		}

		// Then, add initiate transaction
		let initiate_tx_request = signed_order.to_initiate_tx(
			self.config.filler_recipient,
			self.config.order_origin_address,
		);
		rollup_tx_requests.push(initiate_tx_request);

		// Sign and encode all transactions together (ensures correct nonce ordering)
		let rollup_txs = self.sign_and_encode_txns(rollup_tx_requests).await?;

		tracing::info!(
			rollup_txs_count = rollup_txs.len(),
			"Created rollup transactions (fill + initiate)"
		);

		// Get host chain fill (this goes in host_fills field, not in rollup txs)
		let host_fills = signed_fills.get(&self.config.host_chain_id).cloned();

		// --- (2) Target Block Number만 변경하며 10개의 Bundle 생성

		let mut bundles = Vec::with_capacity(NUM_TARGET_BLOCKS as usize);
		sleep(Duration::from_secs(2)).await;

		for i in 1..=NUM_TARGET_BLOCKS {
			let target_block = current_block + i;

			tracing::debug!(
				current_block = current_block,
				target_block = target_block,
				i = i,
				has_host_fills = host_fills.is_some(),
				rollup_txs_count = rollup_txs.len(),
				"Creating bundle for target block"
			);

			let bundle = SignetEthBundle {
				bundle: EthSendBundle {
					txs: rollup_txs.clone(), // All rollup transactions (fill + initiate)
					block_number: target_block,
					min_timestamp: None,
					max_timestamp: None,
					reverting_tx_hashes: vec![],
					replacement_uuid: None,
					..Default::default()
				},
				host_fills: host_fills.clone(), // Host chain fill
				host_txs: vec![],
			};
			bundles.push(bundle);
		}

		Ok(bundles)
	}

	/// Signs and encodes multiple transaction requests into RLP bytes.
	///
	/// This follows the SDK's sign_and_encode_txns pattern:
	/// 1. Set transaction fields (from, gas_limit, priority_fee) for each tx
	/// 2. Use provider.fill() to populate remaining fields (nonce, gas price, etc.)
	/// 3. Encode each signed envelope to bytes
	///
	/// CRITICAL: This method ensures correct nonce ordering by processing all
	/// transactions sequentially with the same provider instance.
	async fn sign_and_encode_txns(
		&self,
		tx_requests: Vec<alloy_rpc_types::TransactionRequest>,
	) -> Result<Vec<Bytes>, DeliveryError> {
		// Get network config for RPC URL
		let network_config = self
			.networks
			.get(&self.config.rollup_chain_id)
			.ok_or_else(|| {
				DeliveryError::Network(format!(
					"No network config for rollup chain {}",
					self.config.rollup_chain_id
				))
			})?;

		let rpc_url = network_config
			.rpc_urls
			.first()
			.and_then(|rpc| rpc.http.as_ref())
			.ok_or_else(|| {
				DeliveryError::Network(format!(
					"No HTTP RPC URL for chain {}",
					self.config.rollup_chain_id
				))
			})?;

		// Create provider with wallet (needed for fill method)
		// IMPORTANT: Use the same provider for all transactions to ensure correct nonce ordering
		let wallet = EthereumWallet::from(self.signer.clone());
		let provider = ProviderBuilder::new().wallet(wallet).connect_http(
			rpc_url
				.parse()
				.map_err(|e| DeliveryError::Network(format!("Invalid RPC URL: {}", e)))?,
		);

		let mut encoded_txs = Vec::new();

		// Process each transaction sequentially to ensure correct nonce ordering
		for mut tx in tx_requests {
			// Fill out the transaction fields (following SDK pattern)
			use alloy_network::TransactionBuilder;
			tx = tx
				.with_from(self.signer.address())
				.with_gas_limit(DEFAULT_GAS_LIMIT)
				.with_max_priority_fee_per_gas(
					(GWEI_TO_WEI * DEFAULT_PRIORITY_FEE_MULTIPLIER) as u128,
				);

			// Use provider.fill() to populate remaining fields (nonce, gas price, chain_id, etc.)
			use alloy_provider::SendableTx;
			let sendable = provider.fill(tx).await.map_err(|e| {
				DeliveryError::Network(format!("Failed to fill transaction: {}", e))
			})?;

			let filled_envelope = match sendable {
				SendableTx::Envelope(envelope) => envelope,
				_ => {
					return Err(DeliveryError::Network(
						"Expected transaction envelope from provider.fill()".to_string(),
					))
				},
			};

			// Encode the signed transaction to RLP bytes (EIP-2718 format)
			let encoded = filled_envelope.encoded_2718();
			encoded_txs.push(Bytes::from(encoded));
		}

		Ok(encoded_txs)
	}

	/// Creates SignedFills for all target chains from the order's outputs.
	///
	/// This follows the SDK's sign_fills pattern:
	/// 1. Aggregate orders
	/// 2. Create UnsignedFill with deadline
	/// 3. Configure with chain addresses
	/// 4. Sign for each target chain
	async fn create_signed_fills(
		&self,
		signed_order: &signet_types::SignedOrder,
	) -> Result<std::collections::HashMap<u64, SignedFill>, DeliveryError> {
		// Get deadline from the order's permit
		let deadline = signed_order
			.permit
			.permit
			.deadline
			.to_string()
			.parse::<u64>()
			.map_err(|e| {
				DeliveryError::Network(format!("Invalid deadline in order permit: {}", e))
			})?;

		// 1. Create AggregateOrders from the SignedOrder
		let mut agg_orders = signet_types::AggregateOrders::new();
		agg_orders.ingest_signed(signed_order);

		// Get all target chain IDs from the order
		let target_chain_ids: std::collections::HashSet<u64> = signed_order
			.outputs
			.iter()
			.map(|output| output.chainId as u64)
			.collect();

		tracing::debug!(
			rollup_chain_id = self.config.rollup_chain_id,
			host_chain_id = self.config.host_chain_id,
			target_chain_ids = ?target_chain_ids,
			deadline = deadline,
			"Creating SignedFills for all target chains"
		);

		// 2. Create UnsignedFill with deadline and rollup chain ID
		let mut unsigned_fill = signet_types::UnsignedFill::new(&agg_orders)
			.with_deadline(deadline)
			.with_ru_chain_id(self.config.rollup_chain_id);

		// 3. Configure with order contract addresses for each chain
		for chain_id in &target_chain_ids {
			let order_address = if *chain_id == self.config.rollup_chain_id {
				self.config.order_origin_address
			} else if *chain_id == self.config.host_chain_id {
				self.config.order_destination_address
			} else {
				// For other chains, we need to look up the address
				// For now, we'll skip them
				tracing::warn!(
					chain_id = chain_id,
					"No order contract address configured for chain"
				);
				continue;
			};

			unsigned_fill = unsigned_fill.with_chain(*chain_id, order_address.into());
		}

		// 4. Sign the fill, producing SignedFills for each target chain
		let signed_fills = unsigned_fill
			.sign(&self.signer)
			.await
			.map_err(|e| DeliveryError::Network(format!("Failed to sign fills: {}", e)))?;

		tracing::info!(
			signed_fills_count = signed_fills.len(),
			chain_ids = ?signed_fills.keys().collect::<Vec<_>>(),
			"Successfully created SignedFills for all target chains"
		);

		Ok(signed_fills)
	}
}

/// Configuration schema for Signet bundle delivery.
pub struct SignetBundleDeliverySchema;

impl SignetBundleDeliverySchema {
	/// Static validation method for use before instance creation
	pub fn validate_config(config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		let instance = Self;
		instance.validate(config)
	}
}

impl ConfigSchema for SignetBundleDeliverySchema {
	fn validate(&self, config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		let schema = Schema::new(
			// Required fields
			vec![
				Field::new("chain_name", FieldType::String),
				Field::new(
					"rollup_chain_id",
					FieldType::Integer {
						min: Some(1),
						max: None,
					},
				),
				Field::new(
					"host_chain_id",
					FieldType::Integer {
						min: Some(1),
						max: None,
					},
				),
				Field::new("order_origin_address", FieldType::String),
				Field::new("order_destination_address", FieldType::String),
				Field::new("filler_recipient", FieldType::String),
			],
			// Optional fields
			vec![Field::new(
				"target_block",
				FieldType::Integer {
					min: Some(1),
					max: None,
				},
			)],
		);

		schema.validate(config)
	}
}

#[async_trait]
impl DeliveryInterface for SignetBundleDelivery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(SignetBundleDeliverySchema)
	}

	async fn submit(&self, tx: SolverTransaction) -> Result<TransactionHash, DeliveryError> {
		// Create bundles from transaction
		let bundles = self.create_bundles(&tx).await?;

		let mut last_bundle_id = String::new();
		let bundles_count = bundles.len();

		tracing::info!(
			bundles_count = bundles_count,
			"Created {} bundles targeting subsequent blocks. Submitting to cache.",
			bundles_count
		);

		// 2. 생성된 모든 번들을 캐시에 순차적으로 제출합니다.
		for (i, bundle) in bundles.into_iter().enumerate() {
			let block_number = bundle.bundle.block_number;

			tracing::debug!(
				attempt = i + 1,
				block_number = block_number,
				txs_count = bundle.bundle.txs.len(),
				has_host_fills = bundle.host_fills.is_some(),
				"Submitting bundle to Signet cache"
			);

			// Submit bundle to cache
			let response = self
				.cache_client
				.forward_bundle(bundle)
				.await
				.map_err(|e| {
					let error_msg =
						format!("Failed to submit bundle for block {}: {}", block_number, e);
					tracing::error!(
						error = %e,
						"Bundle submission failed"
					);
					return DeliveryError::Network(error_msg);
				})?;

			last_bundle_id = response.id.to_string();
			tracing::info!(
				bundle_id = %last_bundle_id,
				block_number = block_number,
				"Bundle successfully submitted to cache"
			);
		}

		// 마지막으로 제출된 번들의 ID를 반환합니다.
		let bundle_id_bytes = last_bundle_id.as_bytes().to_vec();
		Ok(TransactionHash(bundle_id_bytes))
	}

	async fn wait_for_confirmation(
		&self,
		_hash: &TransactionHash,
		_chain_id: u64,
		_confirmations: u64,
	) -> Result<TransactionReceipt, DeliveryError> {
		// TODO: Implement bundle status checking
		// For now, return error as this is not yet implemented
		Err(DeliveryError::Network(
			"Bundle confirmation tracking not yet implemented for Signet".to_string(),
		))
	}

	async fn get_receipt(
		&self,
		_hash: &TransactionHash,
		_chain_id: u64,
	) -> Result<TransactionReceipt, DeliveryError> {
		// TODO: Implement bundle receipt retrieval
		Err(DeliveryError::Network(
			"Bundle receipt retrieval not yet implemented for Signet".to_string(),
		))
	}

	async fn get_gas_price(&self, _chain_id: u64) -> Result<String, DeliveryError> {
		// Signet doesn't use traditional gas pricing
		Ok("0".to_string())
	}

	async fn get_balance(
		&self,
		_address: &str,
		_token: Option<&str>,
		_chain_id: u64,
	) -> Result<String, DeliveryError> {
		// TODO: Implement balance checking if needed
		Err(DeliveryError::Network(
			"Balance checking not yet implemented for Signet".to_string(),
		))
	}

	async fn get_allowance(
		&self,
		_owner: &str,
		_spender: &str,
		_token_address: &str,
		_chain_id: u64,
	) -> Result<String, DeliveryError> {
		// TODO: Implement allowance checking if needed
		Err(DeliveryError::Network(
			"Allowance checking not yet implemented for Signet".to_string(),
		))
	}

	async fn get_nonce(&self, _address: &str, _chain_id: u64) -> Result<u64, DeliveryError> {
		// Signet doesn't use traditional nonces for bundles
		Ok(0)
	}

	async fn get_block_number(&self, chain_id: u64) -> Result<u64, DeliveryError> {
		// Get RPC URL from network config
		let network_config = self.networks.get(&chain_id);

		if let Some(config) = network_config {
			if let Some(rpc_url) = config.rpc_urls.first().and_then(|rpc| rpc.http.as_ref()) {
				// Try to fetch block number from RPC
				if let Ok(url) = rpc_url.parse::<reqwest::Url>() {
					let provider = ProviderBuilder::new()
						.network::<alloy_network::AnyNetwork>()
						.on_http(url);

					match provider.get_block_number().await {
						Ok(block_number) => {
							tracing::debug!(
								chain_id = chain_id,
								block_number = block_number,
								"Retrieved Signet block number from RPC"
							);
							return Ok(block_number);
						},
						Err(e) => {
							tracing::warn!(
								chain_id = chain_id,
								error = %e,
								"Failed to fetch Signet block number from RPC, using fallback"
							);
						},
					}
				}
			}
		}

		// Fall back to config target_block or default if RPC fails
		Ok(self.config.target_block.unwrap_or(DEFAULT_BLOCK_NUMBER))
	}

	async fn estimate_gas(&self, _tx: SolverTransaction) -> Result<u64, DeliveryError> {
		// Signet bundles don't use traditional gas estimation
		Ok(0)
	}

	async fn eth_call(&self, _tx: SolverTransaction) -> Result<Bytes, DeliveryError> {
		// TODO: Implement contract calls if needed
		Err(DeliveryError::Network(
			"Contract calls not yet implemented for Signet".to_string(),
		))
	}
}

/// Factory function to create a Signet bundle delivery from configuration.
pub fn create_delivery(
	config: &toml::Value,
	networks: &NetworksConfig,
	default_private_key: &solver_types::SecretString,
	network_private_keys: &std::collections::HashMap<u64, solver_types::SecretString>,
) -> Result<Box<dyn DeliveryInterface>, DeliveryError> {
	// Validate configuration first
	SignetBundleDeliverySchema::validate_config(config).map_err(|e| {
		DeliveryError::Network(format!(
			"Invalid Signet bundle delivery configuration: {}",
			e
		))
	})?;

	// Parse chain_name (required)
	let chain_name = config
		.get("chain_name")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DeliveryError::Network("chain_name is required".to_string()))?
		.to_string();

	// Parse target_block (optional)
	let target_block = config
		.get("target_block")
		.and_then(|v| v.as_integer())
		.map(|v| v as u64);

	// Parse rollup_chain_id (required)
	let rollup_chain_id = config
		.get("rollup_chain_id")
		.and_then(|v| v.as_integer())
		.ok_or_else(|| DeliveryError::Network("rollup_chain_id is required".to_string()))?
		as u64;

	// Parse host_chain_id (required)
	let host_chain_id = config
		.get("host_chain_id")
		.and_then(|v| v.as_integer())
		.ok_or_else(|| DeliveryError::Network("host_chain_id is required".to_string()))?
		as u64;

	// Parse order_origin_address (required)
	let order_origin_address = config
		.get("order_origin_address")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DeliveryError::Network("order_origin_address is required".to_string()))?
		.parse::<alloy_primitives::Address>()
		.map_err(|e| DeliveryError::Network(format!("Invalid order_origin_address: {}", e)))?;

	// Parse order_destination_address (required)
	let order_destination_address = config
		.get("order_destination_address")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DeliveryError::Network("order_destination_address is required".to_string()))?
		.parse::<alloy_primitives::Address>()
		.map_err(|e| DeliveryError::Network(format!("Invalid order_destination_address: {}", e)))?;

	// Parse filler_recipient (required)
	let filler_recipient = config
		.get("filler_recipient")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DeliveryError::Network("filler_recipient is required".to_string()))?
		.parse::<alloy_primitives::Address>()
		.map_err(|e| DeliveryError::Network(format!("Invalid filler_recipient: {}", e)))?;

	// Create signer from private key
	// Use host chain specific key if available, otherwise use default
	let private_key = network_private_keys
		.get(&host_chain_id)
		.unwrap_or(default_private_key);

	let signer = private_key
		.expose_secret()
		.parse::<PrivateKeySigner>()
		.map_err(|e| DeliveryError::Network(format!("Invalid private key: {}", e)))?;

	let delivery_config = SignetBundleConfig {
		chain_name,
		target_block,
		rollup_chain_id,
		host_chain_id,
		order_origin_address,
		order_destination_address,
		filler_recipient,
	};

	let delivery = SignetBundleDelivery::new(delivery_config, networks.clone(), signer)?;
	Ok(Box::new(delivery))
}

/// Registry for the Signet bundle delivery implementation.
pub struct Registry;

impl solver_types::ImplementationRegistry for Registry {
	const NAME: &'static str = "signet_bundle";
	type Factory = crate::DeliveryFactory;

	fn factory() -> Self::Factory {
		create_delivery
	}
}

impl crate::DeliveryRegistry for Registry {}

#[cfg(test)]
mod tests {
	use super::*;
	use solver_types::utils::tests::builders::NetworksConfigBuilder;
	use std::collections::HashMap;

	fn create_test_networks() -> NetworksConfig {
		NetworksConfigBuilder::new().build()
	}

	fn create_test_config() -> HashMap<&'static str, toml::Value> {
		HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("rollup_chain_id", toml::Value::Integer(901)),
			("host_chain_id", toml::Value::Integer(1)),
			(
				"order_origin_address",
				toml::Value::String("0x0000000000000000000000000000000000000001".to_string()),
			),
			(
				"order_destination_address",
				toml::Value::String("0x0000000000000000000000000000000000000002".to_string()),
			),
			(
				"filler_recipient",
				toml::Value::String("0x0000000000000000000000000000000000000003".to_string()),
			),
		])
	}

	#[test]
	fn test_config_schema_validation_valid() {
		let config = toml::Value::try_from(create_test_config()).unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_with_target_block() {
		let mut config = create_test_config();
		config.insert("target_block", toml::Value::Integer(100));
		let config = toml::Value::try_from(config).unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_missing_chain_name() {
		let mut config = create_test_config();
		config.remove("chain_name");
		let config = toml::Value::try_from(config).unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_config_schema_validation_missing_required_field() {
		let mut config = create_test_config();
		config.remove("order_origin_address");
		let config = toml::Value::try_from(config).unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_create_delivery_valid() {
		let config = toml::Value::try_from(create_test_config()).unwrap();

		let networks = create_test_networks();
		// Use a valid hex private key for testing
		let default_key = solver_types::SecretString::from(
			"0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
		);
		let network_keys = HashMap::new();

		let result = create_delivery(&config, &networks, &default_key, &network_keys);
		assert!(result.is_ok());
	}

	#[test]
	fn test_registry_name() {
		assert_eq!(
			<Registry as solver_types::ImplementationRegistry>::NAME,
			"signet_bundle"
		);
	}
}
