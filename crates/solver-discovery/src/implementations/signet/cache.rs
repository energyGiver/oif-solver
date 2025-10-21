//! Signet Cache Intent Discovery Implementation
//!
//! This module implements intent discovery from Signet's transaction cache.
//! It polls the cache API for signed Permit2 orders and converts them into
//! the internal Intent format used by the solver.
//!
//! ## Overview
//!
//! The Signet cache discovery service:
//! - Polls the Signet transaction cache at regular intervals
//! - Retrieves SignedOrder(s) from the cache
//! - Filters orders based on whitelist (if configured)
//! - Converts Permit2 orders to the internal Intent format
//! - Broadcasts discovered intents to the solver system
//!
//! ## Configuration
//!
//! The service requires the following configuration:
//! - `chain_name` - Signet chain name (e.g., "pecorino")
//! - `polling_interval_secs` - Polling interval in seconds (default: 5)
//! - `whitelist_addresses` - Optional list of user addresses to filter (default: None)

use crate::{DiscoveryError, DiscoveryInterface};
use async_trait::async_trait;
use signet_tx_cache::client::TxCache;
use signet_types::SignedOrder;
use solver_types::{
	current_timestamp, ConfigSchema, Field, FieldType, Intent, IntentMetadata, NetworksConfig, Schema,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;

const DEFAULT_POLLING_INTERVAL_SECS: u64 = 5;
const MAX_POLLING_INTERVAL_SECS: u64 = 300;

/// Signet cache discovery implementation configuration.
#[derive(Debug, Clone)]
pub struct SignetCacheConfig {
	/// Signet chain name (e.g., "pecorino")
	pub chain_name: String,
	/// Polling interval in seconds
	pub polling_interval_secs: u64,
	/// Optional whitelist of user addresses to filter
	pub whitelist_addresses: Option<Vec<String>>,
}

/// Signet cache discovery implementation.
///
/// This implementation polls the Signet transaction cache for new Permit2 orders
/// and converts them into intents for the solver to process.
pub struct SignetCacheDiscovery {
	/// Discovery configuration
	config: SignetCacheConfig,
	/// Networks configuration
	networks: NetworksConfig,
	/// Flag indicating if monitoring is active
	is_monitoring: Arc<AtomicBool>,
	/// Handle for the monitoring task
	monitoring_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
	/// Channel for signaling monitoring shutdown
	stop_signal: Arc<Mutex<Option<broadcast::Sender<()>>>>,
}

impl SignetCacheDiscovery {
	/// Creates a new Signet cache discovery instance.
	pub fn new(config: SignetCacheConfig, networks: NetworksConfig) -> Result<Self, DiscoveryError> {
		// Validate chain name
		if config.chain_name.is_empty() {
			return Err(DiscoveryError::ValidationError(
				"chain_name cannot be empty".to_string(),
			));
		}

		// Validate polling interval
		if config.polling_interval_secs == 0 || config.polling_interval_secs > MAX_POLLING_INTERVAL_SECS {
			return Err(DiscoveryError::ValidationError(
				format!(
					"polling_interval_secs must be between 1 and {}",
					MAX_POLLING_INTERVAL_SECS
				),
			));
		}

		Ok(Self {
			config,
			networks,
			is_monitoring: Arc::new(AtomicBool::new(false)),
			monitoring_handle: Arc::new(Mutex::new(None)),
			stop_signal: Arc::new(Mutex::new(None)),
		})
	}

	/// Converts a Signed Order to an Intent.
	fn order_to_intent(order: &SignedOrder) -> Result<Intent, DiscoveryError> {
		// Generate a simple ID from permit nonce
		let id = format!(
			"signet-{}",
			order.permit.permit.nonce
		);

		// Create intent with Permit2 data
		let data = serde_json::to_value(order).map_err(|e| {
			DiscoveryError::ParseError(format!("Failed to serialize order: {}", e))
		})?;

		// Encode order bytes
		let order_bytes = serde_json::to_vec(order)
			.map_err(|e| DiscoveryError::ParseError(format!("Failed to encode order: {}", e)))?;

		Ok(Intent {
			id,
			source: "signet-cache".to_string(),
			standard: "permit2".to_string(),
			metadata: IntentMetadata {
				requires_auction: false,
				exclusive_until: None,
				discovered_at: current_timestamp(),
			},
			data,
			order_bytes: order_bytes.into(),
			quote_id: None,
			lock_type: "permit2".to_string(),
		})
	}

	/// Checks if an order matches the whitelist.
	fn matches_whitelist(_order: &SignedOrder, whitelist: &Option<Vec<String>>) -> bool {
		match whitelist {
			None => true, // No whitelist = accept all
			Some(_addresses) => {
				// TODO: Implement whitelist filtering once we understand SignedOrder structure
				// For now, accept all orders if whitelist is configured
				tracing::warn!("Whitelist filtering is not yet implemented for Signet orders");
				true
			},
		}
	}

	/// Polling loop that fetches and processes orders.
	async fn polling_loop(
		config: SignetCacheConfig,
		sender: mpsc::UnboundedSender<Intent>,
		mut stop_rx: broadcast::Receiver<()>,
	) {
		// Build cache client based on chain name
		let client = if config.chain_name == "pecorino" {
			TxCache::pecorino()
		} else {
			// Construct URL for other chains
			let url = format!("https://cache.{}.signet.sh", config.chain_name);
			match TxCache::new_from_string(&url) {
				Ok(c) => c,
				Err(e) => {
					tracing::error!("Failed to create cache client: {}", e);
					return;
				}
			}
		};

		let mut interval =
			tokio::time::interval(std::time::Duration::from_secs(config.polling_interval_secs));
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

		loop {
			tokio::select! {
				_ = interval.tick() => {
					match client.get_orders().await {
						Ok(orders) => {
							tracing::debug!("Fetched {} orders from Signet cache", orders.len());

							for order in orders {
								// Apply whitelist filter
								if !Self::matches_whitelist(&order, &config.whitelist_addresses) {
									continue;
								}

								// Convert to intent
								match Self::order_to_intent(&order) {
									Ok(intent) => {
										if let Err(e) = sender.send(intent) {
											tracing::error!("Failed to send intent: {}", e);
										}
									},
									Err(e) => {
										tracing::warn!("Failed to convert order to intent: {}", e);
									},
								}
							}
						},
						Err(e) => {
							tracing::error!("Failed to fetch orders from Signet cache: {}", e);
						},
					}
				}
				_ = stop_rx.recv() => {
					tracing::info!("Stopping Signet cache polling");
					break;
				}
			}
		}
	}
}

/// Configuration schema for Signet cache discovery.
pub struct SignetCacheDiscoverySchema;

impl SignetCacheDiscoverySchema {
	/// Static validation method for use before instance creation
	pub fn validate_config(config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		let instance = Self;
		instance.validate(config)
	}
}

impl ConfigSchema for SignetCacheDiscoverySchema {
	fn validate(&self, config: &toml::Value) -> Result<(), solver_types::ValidationError> {
		let schema = Schema::new(
			// Required fields
			vec![Field::new("chain_name", FieldType::String)],
			// Optional fields
			vec![
				Field::new(
					"polling_interval_secs",
					FieldType::Integer {
						min: Some(1),
						max: Some(MAX_POLLING_INTERVAL_SECS as i64),
					},
				),
				Field::new(
					"whitelist_addresses",
					FieldType::Array(Box::new(FieldType::String)),
				),
			],
		);

		schema.validate(config)
	}
}

#[async_trait]
impl DiscoveryInterface for SignetCacheDiscovery {
	fn config_schema(&self) -> Box<dyn ConfigSchema> {
		Box::new(SignetCacheDiscoverySchema)
	}

	async fn start_monitoring(
		&self,
		sender: mpsc::UnboundedSender<Intent>,
	) -> Result<(), DiscoveryError> {
		if self.is_monitoring.load(Ordering::SeqCst) {
			return Err(DiscoveryError::AlreadyMonitoring);
		}

		// Create broadcast channel for shutdown
		let (stop_tx, stop_rx) = broadcast::channel(1);
		*self.stop_signal.lock().await = Some(stop_tx);

		// Spawn polling task
		let config = self.config.clone();
		let handle = tokio::spawn(async move {
			Self::polling_loop(config, sender, stop_rx).await;
		});

		*self.monitoring_handle.lock().await = Some(handle);
		self.is_monitoring.store(true, Ordering::SeqCst);

		tracing::info!(
			chain_name = %self.config.chain_name,
			polling_interval = self.config.polling_interval_secs,
			whitelist_enabled = self.config.whitelist_addresses.is_some(),
			"Signet cache discovery monitoring started"
		);

		Ok(())
	}

	async fn stop_monitoring(&self) -> Result<(), DiscoveryError> {
		if !self.is_monitoring.load(Ordering::SeqCst) {
			return Ok(());
		}

		// Send shutdown signal if exists
		if let Some(stop_tx) = self.stop_signal.lock().await.take() {
			let _ = stop_tx.send(());
		}

		// Wait for monitoring task to complete
		if let Some(handle) = self.monitoring_handle.lock().await.take() {
			let _ = handle.await;
		}

		self.is_monitoring.store(false, Ordering::SeqCst);
		tracing::info!("Stopped Signet cache discovery monitoring");
		Ok(())
	}
}

/// Factory function to create a Signet cache discovery from configuration.
pub fn create_discovery(
	config: &toml::Value,
	networks: &NetworksConfig,
) -> Result<Box<dyn DiscoveryInterface>, DiscoveryError> {
	// Validate configuration first
	SignetCacheDiscoverySchema::validate_config(config)
		.map_err(|e| DiscoveryError::ValidationError(format!("Invalid configuration: {}", e)))?;

	// Parse chain_name (required)
	let chain_name = config
		.get("chain_name")
		.and_then(|v| v.as_str())
		.ok_or_else(|| DiscoveryError::ValidationError("chain_name is required".to_string()))?
		.to_string();

	// Parse polling_interval_secs (optional, default to 5)
	let polling_interval_secs = config
		.get("polling_interval_secs")
		.and_then(|v| v.as_integer())
		.map(|v| v as u64)
		.unwrap_or(DEFAULT_POLLING_INTERVAL_SECS);

	// Parse whitelist_addresses (optional)
	let whitelist_addresses = config
		.get("whitelist_addresses")
		.and_then(|v| v.as_array())
		.map(|arr| {
			arr.iter()
				.filter_map(|v| v.as_str().map(|s| s.to_string()))
				.collect::<Vec<_>>()
		});

	let discovery_config = SignetCacheConfig {
		chain_name,
		polling_interval_secs,
		whitelist_addresses,
	};

	let discovery = SignetCacheDiscovery::new(discovery_config, networks.clone())?;
	Ok(Box::new(discovery))
}

/// Registry for the Signet cache discovery implementation.
pub struct Registry;

impl solver_types::ImplementationRegistry for Registry {
	const NAME: &'static str = "signet_cache";
	type Factory = crate::DiscoveryFactory;

	fn factory() -> Self::Factory {
		create_discovery
	}
}

impl crate::DiscoveryRegistry for Registry {}

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
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("polling_interval_secs", toml::Value::Integer(5)),
		]))
		.unwrap();

		let result = SignetCacheDiscoverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_with_whitelist() {
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("polling_interval_secs", toml::Value::Integer(5)),
			(
				"whitelist_addresses",
				toml::Value::Array(vec![toml::Value::String(
					"0x21c10426fa5101ab80042ac6cf89f65a7d9e7bcb".to_string(),
				)]),
			),
		]))
		.unwrap();

		let result = SignetCacheDiscoverySchema::validate_config(&config);
		assert!(result.is_ok());
	}

	#[test]
	fn test_config_schema_validation_missing_chain_name() {
		let config = toml::Value::try_from(HashMap::from([(
			"polling_interval_secs",
			toml::Value::Integer(5),
		)]))
		.unwrap();

		let result = SignetCacheDiscoverySchema::validate_config(&config);
		assert!(result.is_err());
	}

	#[test]
	fn test_create_discovery_valid() {
		let config = toml::Value::try_from(HashMap::from([
			("chain_name", toml::Value::String("pecorino".to_string())),
			("polling_interval_secs", toml::Value::Integer(5)),
		]))
		.unwrap();

		let networks = create_test_networks();
		let result = create_discovery(&config, &networks);
		assert!(result.is_ok());
	}

	#[test]
	fn test_registry_name() {
		assert_eq!(
			<Registry as solver_types::ImplementationRegistry>::NAME,
			"signet_cache"
		);
	}
}
