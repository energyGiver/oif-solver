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
use alloy_primitives::Bytes;
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

const DEFAULT_BLOCK_NUMBER: u64 = 1;

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
	_networks: NetworksConfig,
	/// Signet cache client
	cache_client: Arc<TxCache>,
	/// Solver's signer for creating SignedFills
	#[allow(dead_code)] // Used in TODO: create_signed_fill implementation
	signer: PrivateKeySigner,
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
			return Err(DeliveryError::Network("chain_name cannot be empty".to_string()));
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
			_networks: networks,
			cache_client: Arc::new(cache_client),
			signer,
		})
	}

	/// Creates a bundle from a solver fill transaction.
	///
	/// Extracts the SignedOrder from transaction metadata and creates a proper
	/// Signet bundle with the order initiation transaction and SignedFill.
	async fn create_bundle(&self, tx: &SolverTransaction) -> Result<SignetEthBundle, DeliveryError> {
		let target_block = self.config.target_block.unwrap_or(DEFAULT_BLOCK_NUMBER);

		// Extract SignedOrder from transaction metadata
		let signed_order = if let Some(metadata) = &tx.metadata {
			// Deserialize SignedOrder from metadata
			serde_json::from_value::<signet_types::SignedOrder>(metadata.clone()).map_err(|e| {
				DeliveryError::Network(format!("Failed to deserialize SignedOrder from metadata: {}", e))
			})?
		} else {
			// No metadata - this is not a Signet order, create empty bundle
			tracing::warn!("No metadata in transaction, creating empty bundle");
			return Ok(SignetEthBundle {
				bundle: EthSendBundle {
					txs: vec![],
					block_number: target_block,
					min_timestamp: None,
					max_timestamp: None,
					reverting_tx_hashes: vec![],
					replacement_uuid: None,
					..Default::default()
				},
				host_fills: None,
				host_txs: vec![],
			});
		};

		// Create initiate order transaction for L2
		// This transaction calls OrderOrigin.initiatePermit2() with the SignedOrder
		let initiate_tx_request = signed_order.to_initiate_tx(
			self.config.filler_recipient,
			self.config.order_origin_address,
		);

		// Encode the transaction request as bytes
		// For Signet bundles, we send the transaction request bytes (not a signed transaction)
		let initiate_tx_bytes = Bytes::from(
			initiate_tx_request
				.input
				.input
				.map(|b| b.to_vec())
				.unwrap_or_default(),
		);

		// Create SignedFill for host (L1) outputs
		// The filler signs a permit2 approval for the tokens they will send
		let host_fills = self.create_signed_fill(&signed_order).await?;

		// Create bundle with order initiation transaction and signed fill
		let bundle = SignetEthBundle {
			bundle: EthSendBundle {
				txs: vec![initiate_tx_bytes], // L2 transaction to initiate the order
				block_number: target_block,
				min_timestamp: None,
				max_timestamp: None,
				reverting_tx_hashes: vec![],
				replacement_uuid: None,
				..Default::default()
			},
			host_fills,
			host_txs: vec![], // Empty as per signet-orders example
		};

		Ok(bundle)
	}

	/// Creates a SignedFill from the order's outputs.
	///
	/// The filler creates a permit2 signature authorizing the transfer of tokens
	/// to fulfill the order's outputs on the host (L1) chain.
	///
	/// TODO: Implement full SignedFill creation using UnsignedFill pattern.
	/// This requires converting the SignedOrder's outputs into an AggregateOrders
	/// structure and using UnsignedFill::sign_for() to create the SignedFill.
	/// For now, returns None to allow basic bundle submission without fills.
	async fn create_signed_fill(
		&self,
		_signed_order: &signet_types::SignedOrder,
	) -> Result<Option<SignedFill>, DeliveryError> {
		// TODO: Implement SignedFill creation
		// Steps required:
		// 1. Filter outputs for host chain
		// 2. Create AggregateOrders from the outputs
		// 3. Use UnsignedFill::new(&agg_orders)
		//    .with_ru_chain_id(rollup_chain_id)
		//    .with_chain(host_chain_id, order_destination_address)
		//    .sign_for(host_chain_id, &self.signer)
		//
		// For now, return None (bundle will have no host fills)
		Ok(None)
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
				Field::new("rollup_chain_id", FieldType::Integer { min: Some(1), max: None }),
				Field::new("host_chain_id", FieldType::Integer { min: Some(1), max: None }),
				Field::new("order_origin_address", FieldType::String),
				Field::new("order_destination_address", FieldType::String),
				Field::new("filler_recipient", FieldType::String),
			],
			// Optional fields
			vec![Field::new("target_block", FieldType::Integer { min: Some(1), max: None })],
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
		// Create bundle from transaction
		let bundle = self.create_bundle(&tx).await?;

		// Submit bundle to cache
		let response = self
			.cache_client
			.forward_bundle(bundle)
			.await
			.map_err(|e| DeliveryError::Network(format!("Failed to submit bundle: {}", e)))?;

		// Return bundle ID as transaction hash
		// Note: This is not a traditional transaction hash, but a bundle UUID
		let bundle_id_bytes = response.id.as_bytes().to_vec();
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

	async fn get_block_number(&self, _chain_id: u64) -> Result<u64, DeliveryError> {
		// TODO: Implement block number retrieval from Signet cache if available
		Err(DeliveryError::Network(
			"Block number retrieval not yet implemented for Signet".to_string(),
		))
	}

	async fn estimate_gas(&self, _tx: SolverTransaction) -> Result<u64, DeliveryError> {
		// Signet bundles don't use traditional gas estimation
		Ok(0)
	}

	async fn eth_call(&self, _tx: SolverTransaction) -> Result<Bytes, DeliveryError> {
		// TODO: Implement contract calls if needed
		Err(DeliveryError::Network("Contract calls not yet implemented for Signet".to_string()))
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
		DeliveryError::Network(format!("Invalid Signet bundle delivery configuration: {}", e))
	})?;

	// Parse chain_name (required)
	let chain_name = config
		.get("chain_name")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DeliveryError::Network("chain_name is required".to_string()))?
		.to_string();

	// Parse target_block (optional)
	let target_block = config.get("target_block").and_then(|v| v.as_integer()).map(|v| v as u64);

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
		let default_key =
			solver_types::SecretString::from("0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");
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
