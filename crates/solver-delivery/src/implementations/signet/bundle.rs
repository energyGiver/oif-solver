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
use async_trait::async_trait;
use signet_bundle::SignetEthBundle;
use signet_tx_cache::client::TxCache;
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
}

impl SignetBundleDelivery {
	/// Creates a new Signet bundle delivery instance.
	pub fn new(config: SignetBundleConfig, networks: NetworksConfig) -> Result<Self, DeliveryError> {
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
		})
	}

	/// Creates a bundle from a solver transaction.
	///
	/// This wraps the transaction in a SignetEthBundle with the transaction
	/// as a host (L1) transaction.
	fn create_bundle(&self, tx: &SolverTransaction) -> Result<SignetEthBundle, DeliveryError> {
		// TODO: Properly encode the transaction
		// For now, we create a simple bundle structure
		let target_block = self.config.target_block.unwrap_or(DEFAULT_BLOCK_NUMBER);

		// Create empty bundle (no L2 txs for now)
		let eth_send_bundle = EthSendBundle {
			txs: vec![], // L2 transactions
			block_number: target_block,
			min_timestamp: None,
			max_timestamp: None,
			reverting_tx_hashes: vec![],
			replacement_uuid: None,
			..Default::default()
		};

		// Create host transaction bytes
		// TODO: Properly encode the transaction as signed envelope
		let host_tx_bytes = Bytes::from(tx.data.clone());

		Ok(SignetEthBundle {
			bundle: eth_send_bundle,
			host_fills: None, // TODO: Add proper fills when integrating with discovered orders
			host_txs: vec![host_tx_bytes],
		})
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
			vec![Field::new("chain_name", FieldType::String)],
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
		let bundle = self.create_bundle(&tx)?;

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
	_default_private_key: &solver_types::SecretString,
	_network_private_keys: &std::collections::HashMap<u64, solver_types::SecretString>,
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

	let delivery_config = SignetBundleConfig { chain_name, target_block };

	let delivery = SignetBundleDelivery::new(delivery_config, networks.clone())?;
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

	#[test]
	fn test_config_schema_validation_valid() {
		let config = toml::Value::try_from(HashMap::from([(
			"chain_name",
			toml::Value::String("pecorino".to_string()),
		)]))
		.unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_with_target_block() {
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("target_block", toml::Value::Integer(100)),
		]))
		.unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_missing_chain_name() {
		let config = toml::Value::try_from(HashMap::from([(
			"target_block",
			toml::Value::Integer(100),
		)]))
		.unwrap();

		let result = SignetBundleDeliverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_create_delivery_valid() {
		let config = toml::Value::try_from(HashMap::from([(
			"chain_name",
			toml::Value::String("pecorino".to_string()),
		)]))
		.unwrap();

		let networks = create_test_networks();
		let default_key = solver_types::SecretString::from("test_key");
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
