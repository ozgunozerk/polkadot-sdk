// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

use cumulus_client_cli::CollatorOptions;
use cumulus_client_collator::service::CollatorService;
use cumulus_client_consensus_aura::collators::{
	lookahead::{self as aura, Params as AuraParams},
	slot_based::{self as slot_based, Params as SlotBasedParams},
};
use cumulus_client_consensus_common::ParachainBlockImport as TParachainBlockImport;
use cumulus_client_consensus_proposer::Proposer;
#[allow(deprecated)]
use cumulus_client_service::old_consensus;
use cumulus_client_service::{
	build_network, build_relay_chain_interface, prepare_node_config, start_relay_chain_tasks,
	BuildNetworkParams, CollatorSybilResistance, DARecoveryProfile, StartRelayChainTasksParams,
};
use cumulus_primitives_core::{relay_chain::ValidationCode, ParaId};
use cumulus_relay_chain_interface::{OverseerHandle, RelayChainInterface};
use sc_rpc::DenyUnsafe;

use jsonrpsee::RpcModule;

use crate::{
	common::{
		aura::{AuraIdT, AuraRuntimeApi},
		ConstructNodeRuntimeApi,
	},
	fake_runtime_api::aura::RuntimeApi as FakeRuntimeApi,
	rpc,
};
pub use parachains_common::{AccountId, AuraId, Balance, Block, Hash, Nonce};

use cumulus_client_consensus_relay_chain::Verifier as RelayChainVerifier;
use futures::prelude::*;
use prometheus_endpoint::Registry;
use sc_client_api::BlockchainEvents;
use sc_consensus::{
	import_queue::{BasicQueue, Verifier as VerifierT},
	BlockImportParams, ImportQueue,
};
use sc_executor::{HeapAllocStrategy, WasmExecutor, DEFAULT_HEAP_ALLOC_STRATEGY};
use sc_network::{config::FullNetworkConfiguration, service::traits::NetworkBackend, NetworkBlock};
use sc_service::{Configuration, PartialComponents, TFullBackend, TFullClient, TaskManager};
use sc_telemetry::{Telemetry, TelemetryHandle, TelemetryWorker, TelemetryWorkerHandle};
use sp_api::{ApiExt, ConstructRuntimeApi, ProvideRuntimeApi};
use sp_consensus_aura::AuraApi;
use sp_keystore::KeystorePtr;
use sp_runtime::{app_crypto::AppCrypto, traits::Header as HeaderT};
use std::{marker::PhantomData, sync::Arc, time::Duration};

use polkadot_primitives::CollatorPair;

#[cfg(not(feature = "runtime-benchmarks"))]
type HostFunctions = cumulus_client_service::ParachainHostFunctions;

#[cfg(feature = "runtime-benchmarks")]
type HostFunctions = (
	cumulus_client_service::ParachainHostFunctions,
	frame_benchmarking::benchmarking::HostFunctions,
);

type ParachainClient<RuntimeApi> = TFullClient<Block, RuntimeApi, WasmExecutor<HostFunctions>>;

type ParachainBackend = TFullBackend<Block>;

type ParachainBlockImport<RuntimeApi> =
	TParachainBlockImport<Block, Arc<ParachainClient<RuntimeApi>>, ParachainBackend>;

/// Assembly of PartialComponents (enough to run chain ops subcommands)
pub type Service<RuntimeApi> = PartialComponents<
	ParachainClient<RuntimeApi>,
	ParachainBackend,
	(),
	sc_consensus::DefaultImportQueue<Block>,
	sc_transaction_pool::FullPool<Block, ParachainClient<RuntimeApi>>,
	(ParachainBlockImport<RuntimeApi>, Option<Telemetry>, Option<TelemetryWorkerHandle>),
>;

/// Starts a `ServiceBuilder` for a full service.
///
/// Use this macro if you don't actually need the full service, but just the builder in order to
/// be able to perform chain operations.
pub fn new_partial<RuntimeApi, BIQ>(
	config: &Configuration,
	build_import_queue: BIQ,
) -> Result<Service<RuntimeApi>, sc_service::Error>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<RuntimeApi>>,
	BIQ: FnOnce(
		Arc<ParachainClient<RuntimeApi>>,
		ParachainBlockImport<RuntimeApi>,
		&Configuration,
		Option<TelemetryHandle>,
		&TaskManager,
	) -> Result<sc_consensus::DefaultImportQueue<Block>, sc_service::Error>,
{
	let telemetry = config
		.telemetry_endpoints
		.clone()
		.filter(|x| !x.is_empty())
		.map(|endpoints| -> Result<_, sc_telemetry::Error> {
			let worker = TelemetryWorker::new(16)?;
			let telemetry = worker.handle().new_telemetry(endpoints);
			Ok((worker, telemetry))
		})
		.transpose()?;

	let heap_pages = config
		.default_heap_pages
		.map_or(DEFAULT_HEAP_ALLOC_STRATEGY, |h| HeapAllocStrategy::Static { extra_pages: h as _ });

	let executor = sc_executor::WasmExecutor::<HostFunctions>::builder()
		.with_execution_method(config.wasm_method)
		.with_max_runtime_instances(config.max_runtime_instances)
		.with_runtime_cache_size(config.runtime_cache_size)
		.with_onchain_heap_alloc_strategy(heap_pages)
		.with_offchain_heap_alloc_strategy(heap_pages)
		.build();

	let (client, backend, keystore_container, task_manager) =
		sc_service::new_full_parts_record_import::<Block, RuntimeApi, _>(
			config,
			telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
			executor,
			true,
		)?;
	let client = Arc::new(client);

	let telemetry_worker_handle = telemetry.as_ref().map(|(worker, _)| worker.handle());

	let telemetry = telemetry.map(|(worker, telemetry)| {
		task_manager.spawn_handle().spawn("telemetry", None, worker.run());
		telemetry
	});

	let transaction_pool = sc_transaction_pool::BasicPool::new_full(
		config.transaction_pool.clone(),
		config.role.is_authority().into(),
		config.prometheus_registry(),
		task_manager.spawn_essential_handle(),
		client.clone(),
	);

	let block_import = ParachainBlockImport::new(client.clone(), backend.clone());

	let import_queue = build_import_queue(
		client.clone(),
		block_import.clone(),
		config,
		telemetry.as_ref().map(|telemetry| telemetry.handle()),
		&task_manager,
	)?;

	Ok(PartialComponents {
		backend,
		client,
		import_queue,
		keystore_container,
		task_manager,
		transaction_pool,
		select_chain: (),
		other: (block_import, telemetry, telemetry_worker_handle),
	})
}

/// Start a node with the given parachain `Configuration` and relay chain `Configuration`.
///
/// This is the actual implementation that is abstract over the executor and the runtime api.
#[sc_tracing::logging::prefix_logs_with("Parachain")]
async fn start_node_impl<RuntimeApi, RB, BIQ, SC, Net>(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	sybil_resistance_level: CollatorSybilResistance,
	para_id: ParaId,
	rpc_ext_builder: RB,
	build_import_queue: BIQ,
	start_consensus: SC,
	hwbench: Option<sc_sysinfo::HwBench>,
) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient<RuntimeApi>>)>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<RuntimeApi>>,
	RB: Fn(
			DenyUnsafe,
			Arc<ParachainClient<RuntimeApi>>,
			Arc<ParachainBackend>,
			Arc<sc_transaction_pool::FullPool<Block, ParachainClient<RuntimeApi>>>,
		) -> Result<jsonrpsee::RpcModule<()>, sc_service::Error>
		+ 'static,
	BIQ: FnOnce(
		Arc<ParachainClient<RuntimeApi>>,
		ParachainBlockImport<RuntimeApi>,
		&Configuration,
		Option<TelemetryHandle>,
		&TaskManager,
	) -> Result<sc_consensus::DefaultImportQueue<Block>, sc_service::Error>,
	SC: FnOnce(
		Arc<ParachainClient<RuntimeApi>>,
		ParachainBlockImport<RuntimeApi>,
		Option<&Registry>,
		Option<TelemetryHandle>,
		&TaskManager,
		Arc<dyn RelayChainInterface>,
		Arc<sc_transaction_pool::FullPool<Block, ParachainClient<RuntimeApi>>>,
		KeystorePtr,
		Duration,
		ParaId,
		CollatorPair,
		OverseerHandle,
		Arc<dyn Fn(Hash, Option<Vec<u8>>) + Send + Sync>,
		Arc<ParachainBackend>,
	) -> Result<(), sc_service::Error>,
	Net: NetworkBackend<Block, Hash>,
{
	let parachain_config = prepare_node_config(parachain_config);

	let params = new_partial::<RuntimeApi, BIQ>(&parachain_config, build_import_queue)?;
	let (block_import, mut telemetry, telemetry_worker_handle) = params.other;

	let client = params.client.clone();
	let backend = params.backend.clone();

	let mut task_manager = params.task_manager;
	let (relay_chain_interface, collator_key) = build_relay_chain_interface(
		polkadot_config,
		&parachain_config,
		telemetry_worker_handle,
		&mut task_manager,
		collator_options.clone(),
		hwbench.clone(),
	)
	.await
	.map_err(|e| sc_service::Error::Application(Box::new(e) as Box<_>))?;

	let validator = parachain_config.role.is_authority();
	let prometheus_registry = parachain_config.prometheus_registry().cloned();
	let transaction_pool = params.transaction_pool.clone();
	let import_queue_service = params.import_queue.service();
	let net_config = FullNetworkConfiguration::<_, _, Net>::new(&parachain_config.network);

	let (network, system_rpc_tx, tx_handler_controller, start_network, sync_service) =
		build_network(BuildNetworkParams {
			parachain_config: &parachain_config,
			net_config,
			client: client.clone(),
			transaction_pool: transaction_pool.clone(),
			para_id,
			spawn_handle: task_manager.spawn_handle(),
			relay_chain_interface: relay_chain_interface.clone(),
			import_queue: params.import_queue,
			sybil_resistance_level,
		})
		.await?;

	let rpc_builder = {
		let client = client.clone();
		let transaction_pool = transaction_pool.clone();
		let backend_for_rpc = backend.clone();

		Box::new(move |deny_unsafe, _| {
			rpc_ext_builder(
				deny_unsafe,
				client.clone(),
				backend_for_rpc.clone(),
				transaction_pool.clone(),
			)
		})
	};

	sc_service::spawn_tasks(sc_service::SpawnTasksParams {
		rpc_builder,
		client: client.clone(),
		transaction_pool: transaction_pool.clone(),
		task_manager: &mut task_manager,
		config: parachain_config,
		keystore: params.keystore_container.keystore(),
		backend: backend.clone(),
		network: network.clone(),
		sync_service: sync_service.clone(),
		system_rpc_tx,
		tx_handler_controller,
		telemetry: telemetry.as_mut(),
	})?;

	if let Some(hwbench) = hwbench {
		sc_sysinfo::print_hwbench(&hwbench);
		if validator {
			warn_if_slow_hardware(&hwbench);
		}

		if let Some(ref mut telemetry) = telemetry {
			let telemetry_handle = telemetry.handle();
			task_manager.spawn_handle().spawn(
				"telemetry_hwbench",
				None,
				sc_sysinfo::initialize_hwbench_telemetry(telemetry_handle, hwbench),
			);
		}
	}

	let announce_block = {
		let sync_service = sync_service.clone();
		Arc::new(move |hash, data| sync_service.announce_block(hash, data))
	};

	let relay_chain_slot_duration = Duration::from_secs(6);

	let overseer_handle = relay_chain_interface
		.overseer_handle()
		.map_err(|e| sc_service::Error::Application(Box::new(e)))?;

	start_relay_chain_tasks(StartRelayChainTasksParams {
		client: client.clone(),
		announce_block: announce_block.clone(),
		para_id,
		relay_chain_interface: relay_chain_interface.clone(),
		task_manager: &mut task_manager,
		da_recovery_profile: if validator {
			DARecoveryProfile::Collator
		} else {
			DARecoveryProfile::FullNode
		},
		import_queue: import_queue_service,
		relay_chain_slot_duration,
		recovery_handle: Box::new(overseer_handle.clone()),
		sync_service: sync_service.clone(),
	})?;

	if validator {
		start_consensus(
			client.clone(),
			block_import,
			prometheus_registry.as_ref(),
			telemetry.as_ref().map(|t| t.handle()),
			&task_manager,
			relay_chain_interface.clone(),
			transaction_pool,
			params.keystore_container.keystore(),
			relay_chain_slot_duration,
			para_id,
			collator_key.expect("Command line arguments do not allow this. qed"),
			overseer_handle,
			announce_block,
			backend.clone(),
		)?;
	}

	start_network.start_network();

	Ok((task_manager, client))
}

/// Build the import queue for Aura-based runtimes.
pub fn build_aura_import_queue(
	client: Arc<ParachainClient<FakeRuntimeApi>>,
	block_import: ParachainBlockImport<FakeRuntimeApi>,
	config: &Configuration,
	telemetry: Option<TelemetryHandle>,
	task_manager: &TaskManager,
) -> Result<sc_consensus::DefaultImportQueue<Block>, sc_service::Error> {
	let slot_duration = cumulus_client_consensus_aura::slot_duration(&*client)?;

	cumulus_client_consensus_aura::import_queue::<
		sp_consensus_aura::sr25519::AuthorityPair,
		_,
		_,
		_,
		_,
		_,
	>(cumulus_client_consensus_aura::ImportQueueParams {
		block_import,
		client,
		create_inherent_data_providers: move |_, _| async move {
			let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

			let slot =
				sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
					*timestamp,
					slot_duration,
				);

			Ok((slot, timestamp))
		},
		registry: config.prometheus_registry(),
		spawner: &task_manager.spawn_essential_handle(),
		telemetry,
	})
	.map_err(Into::into)
}

/// Start a rococo parachain node.
pub async fn start_rococo_parachain_node<Net: NetworkBackend<Block, Hash>>(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	para_id: ParaId,
	use_experimental_slot_based: bool,
	hwbench: Option<sc_sysinfo::HwBench>,
) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient<FakeRuntimeApi>>)> {
	let consensus_starter = if use_experimental_slot_based {
		start_slot_based_aura_consensus::<_, AuraId>
	} else {
		start_lookahead_aura_consensus::<_, AuraId>
	};
	start_node_impl::<FakeRuntimeApi, _, _, _, Net>(
		parachain_config,
		polkadot_config,
		collator_options,
		CollatorSybilResistance::Resistant, // Aura
		para_id,
		build_parachain_rpc_extensions::<FakeRuntimeApi>,
		build_aura_import_queue,
		consensus_starter,
		hwbench,
	)
	.await
}

/// Build the import queue for the shell runtime.
pub fn build_shell_import_queue(
	client: Arc<ParachainClient<FakeRuntimeApi>>,
	block_import: ParachainBlockImport<FakeRuntimeApi>,
	config: &Configuration,
	_: Option<TelemetryHandle>,
	task_manager: &TaskManager,
) -> Result<sc_consensus::DefaultImportQueue<Block>, sc_service::Error> {
	cumulus_client_consensus_relay_chain::import_queue(
		client,
		block_import,
		|_, _| async { Ok(()) },
		&task_manager.spawn_essential_handle(),
		config.prometheus_registry(),
	)
	.map_err(Into::into)
}

fn build_parachain_rpc_extensions<RuntimeApi>(
	deny_unsafe: sc_rpc::DenyUnsafe,
	client: Arc<ParachainClient<RuntimeApi>>,
	backend: Arc<ParachainBackend>,
	pool: Arc<sc_transaction_pool::FullPool<Block, ParachainClient<RuntimeApi>>>,
) -> Result<jsonrpsee::RpcModule<()>, sc_service::Error>
where
	RuntimeApi: ConstructRuntimeApi<Block, ParachainClient<RuntimeApi>> + Send + Sync + 'static,
	RuntimeApi::RuntimeApi: sp_transaction_pool::runtime_api::TaggedTransactionQueue<Block>
		+ sp_block_builder::BlockBuilder<Block>
		+ pallet_transaction_payment_rpc::TransactionPaymentRuntimeApi<Block, Balance>
		+ substrate_frame_rpc_system::AccountNonceApi<Block, AccountId, Nonce>,
{
	let deps = rpc::FullDeps { client, pool, deny_unsafe };

	rpc::create_full(deps, backend).map_err(Into::into)
}

fn build_contracts_rpc_extensions(
	deny_unsafe: sc_rpc::DenyUnsafe,
	client: Arc<ParachainClient<FakeRuntimeApi>>,
	_backend: Arc<ParachainBackend>,
	pool: Arc<sc_transaction_pool::FullPool<Block, ParachainClient<FakeRuntimeApi>>>,
) -> Result<jsonrpsee::RpcModule<()>, sc_service::Error> {
	let deps = crate::rpc::FullDeps { client: client.clone(), pool: pool.clone(), deny_unsafe };

	crate::rpc::create_contracts_rococo(deps).map_err(Into::into)
}

/// Start a polkadot-shell parachain node.
pub async fn start_shell_node<Net: NetworkBackend<Block, Hash>>(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	para_id: ParaId,
	hwbench: Option<sc_sysinfo::HwBench>,
) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient<FakeRuntimeApi>>)> {
	start_node_impl::<FakeRuntimeApi, _, _, _, Net>(
		parachain_config,
		polkadot_config,
		collator_options,
		CollatorSybilResistance::Unresistant, // free-for-all consensus
		para_id,
		|_, _, _, _| Ok(RpcModule::new(())),
		build_shell_import_queue,
		start_relay_chain_consensus,
		hwbench,
	)
	.await
}

struct Verifier<Client, AuraId> {
	client: Arc<Client>,
	aura_verifier: Box<dyn VerifierT<Block>>,
	relay_chain_verifier: Box<dyn VerifierT<Block>>,
	_phantom: PhantomData<AuraId>,
}

#[async_trait::async_trait]
impl<Client, AuraId> VerifierT<Block> for Verifier<Client, AuraId>
where
	Client: ProvideRuntimeApi<Block> + Send + Sync,
	Client::Api: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	async fn verify(
		&self,
		block_import: BlockImportParams<Block>,
	) -> Result<BlockImportParams<Block>, String> {
		if self.client.runtime_api().has_aura_api(*block_import.header.parent_hash()) {
			self.aura_verifier.verify(block_import).await
		} else {
			self.relay_chain_verifier.verify(block_import).await
		}
	}
}

/// Build the import queue for parachain runtimes that started with relay chain consensus and
/// switched to aura.
pub fn build_relay_to_aura_import_queue<RuntimeApi, AuraId>(
	client: Arc<ParachainClient<RuntimeApi>>,
	block_import: ParachainBlockImport<RuntimeApi>,
	config: &Configuration,
	telemetry_handle: Option<TelemetryHandle>,
	task_manager: &TaskManager,
) -> Result<sc_consensus::DefaultImportQueue<Block>, sc_service::Error>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	let verifier_client = client.clone();

	let aura_verifier = cumulus_client_consensus_aura::build_verifier::<
		<AuraId as AppCrypto>::Pair,
		_,
		_,
		_,
	>(cumulus_client_consensus_aura::BuildVerifierParams {
		client: verifier_client.clone(),
		create_inherent_data_providers: move |parent_hash, _| {
			let cidp_client = verifier_client.clone();
			async move {
				let slot_duration =
					cumulus_client_consensus_aura::slot_duration_at(&*cidp_client, parent_hash)?;
				let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

				let slot =
					sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
						*timestamp,
						slot_duration,
					);

				Ok((slot, timestamp))
			}
		},
		telemetry: telemetry_handle,
	});

	let relay_chain_verifier =
		Box::new(RelayChainVerifier::new(client.clone(), |_, _| async { Ok(()) })) as Box<_>;

	let verifier = Verifier {
		client,
		relay_chain_verifier,
		aura_verifier: Box::new(aura_verifier),
		_phantom: PhantomData,
	};

	let registry = config.prometheus_registry();
	let spawner = task_manager.spawn_essential_handle();

	Ok(BasicQueue::new(verifier, Box::new(block_import), None, &spawner, registry))
}

/// Uses the lookahead collator to support async backing.
///
/// Start an aura powered parachain node. Some system chains use this.
pub async fn start_generic_aura_async_backing_node<Net: NetworkBackend<Block, Hash>>(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	para_id: ParaId,
	use_experimental_slot_based: bool,
	hwbench: Option<sc_sysinfo::HwBench>,
) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient<FakeRuntimeApi>>)> {
	let consensus_starter = if use_experimental_slot_based {
		start_slot_based_aura_consensus::<_, AuraId>
	} else {
		start_lookahead_aura_consensus::<_, AuraId>
	};
	start_node_impl::<FakeRuntimeApi, _, _, _, Net>(
		parachain_config,
		polkadot_config,
		collator_options,
		CollatorSybilResistance::Resistant, // Aura
		para_id,
		build_parachain_rpc_extensions::<FakeRuntimeApi>,
		build_relay_to_aura_import_queue::<_, AuraId>,
		consensus_starter,
		hwbench,
	)
	.await
}

/// Start a shell node which should later transition into an Aura powered parachain node. Asset Hub
/// uses this because at genesis, Asset Hub was on the `shell` runtime which didn't have Aura and
/// needs to sync and upgrade before it can run `AuraApi` functions.
///
/// Uses the lookahead collator to support async backing.
#[sc_tracing::logging::prefix_logs_with("Parachain")]
pub async fn start_asset_hub_async_backing_node<RuntimeApi, AuraId, Net>(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	para_id: ParaId,
	use_experimental_slot_based: bool,
	hwbench: Option<sc_sysinfo::HwBench>,
) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient<RuntimeApi>>)>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>
		+ pallet_transaction_payment_rpc::TransactionPaymentRuntimeApi<Block, Balance>
		+ substrate_frame_rpc_system::AccountNonceApi<Block, AccountId, Nonce>,
	AuraId: AuraIdT + Sync,
	Net: NetworkBackend<Block, Hash>,
{
	let consensus_starter = if use_experimental_slot_based {
		start_slot_based_aura_consensus::<_, AuraId>
	} else {
		start_lookahead_aura_consensus::<_, AuraId>
	};

	start_node_impl::<RuntimeApi, _, _, _, Net>(
		parachain_config,
		polkadot_config,
		collator_options,
		CollatorSybilResistance::Resistant, // Aura
		para_id,
		build_parachain_rpc_extensions,
		build_relay_to_aura_import_queue::<_, AuraId>,
		consensus_starter,
		hwbench,
	)
	.await
}

/// Wait for the Aura runtime API to appear on chain.
/// This is useful for chains that started out without Aura. Components that
/// are depending on Aura functionality will wait until Aura appears in the runtime.
async fn wait_for_aura<RuntimeApi, AuraId>(client: Arc<ParachainClient<RuntimeApi>>)
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	let finalized_hash = client.chain_info().finalized_hash;
	if client
		.runtime_api()
		.has_api::<dyn AuraApi<Block, AuraId>>(finalized_hash)
		.unwrap_or(false)
	{
		return;
	};

	let mut stream = client.finality_notification_stream();
	while let Some(notification) = stream.next().await {
		let has_aura_api = client
			.runtime_api()
			.has_api::<dyn AuraApi<Block, AuraId>>(notification.hash)
			.unwrap_or(false);
		if has_aura_api {
			return;
		}
	}
}

/// Start relay-chain consensus that is free for all. Everyone can submit a block, the relay-chain
/// decides what is backed and included.
fn start_relay_chain_consensus(
	client: Arc<ParachainClient<FakeRuntimeApi>>,
	block_import: ParachainBlockImport<FakeRuntimeApi>,
	prometheus_registry: Option<&Registry>,
	telemetry: Option<TelemetryHandle>,
	task_manager: &TaskManager,
	relay_chain_interface: Arc<dyn RelayChainInterface>,
	transaction_pool: Arc<sc_transaction_pool::FullPool<Block, ParachainClient<FakeRuntimeApi>>>,
	_keystore: KeystorePtr,
	_relay_chain_slot_duration: Duration,
	para_id: ParaId,
	collator_key: CollatorPair,
	overseer_handle: OverseerHandle,
	announce_block: Arc<dyn Fn(Hash, Option<Vec<u8>>) + Send + Sync>,
	_backend: Arc<ParachainBackend>,
) -> Result<(), sc_service::Error> {
	let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
		task_manager.spawn_handle(),
		client.clone(),
		transaction_pool,
		prometheus_registry,
		telemetry,
	);

	let free_for_all = cumulus_client_consensus_relay_chain::build_relay_chain_consensus(
		cumulus_client_consensus_relay_chain::BuildRelayChainConsensusParams {
			para_id,
			proposer_factory,
			block_import,
			relay_chain_interface: relay_chain_interface.clone(),
			create_inherent_data_providers: move |_, (relay_parent, validation_data)| {
				let relay_chain_interface = relay_chain_interface.clone();
				async move {
					let parachain_inherent =
							cumulus_client_parachain_inherent::ParachainInherentDataProvider::create_at(
								relay_parent,
								&relay_chain_interface,
								&validation_data,
								para_id,
							).await;
					let parachain_inherent = parachain_inherent.ok_or_else(|| {
						Box::<dyn std::error::Error + Send + Sync>::from(
							"Failed to create parachain inherent",
						)
					})?;
					Ok(parachain_inherent)
				}
			},
		},
	);

	let spawner = task_manager.spawn_handle();

	// Required for free-for-all consensus
	#[allow(deprecated)]
	old_consensus::start_collator_sync(old_consensus::StartCollatorParams {
		para_id,
		block_status: client.clone(),
		announce_block,
		overseer_handle,
		spawner,
		key: collator_key,
		parachain_consensus: free_for_all,
		runtime_api: client.clone(),
	});

	Ok(())
}

/// Start consensus using the lookahead aura collator.
fn start_lookahead_aura_consensus<RuntimeApi, AuraId>(
	client: Arc<ParachainClient<RuntimeApi>>,
	block_import: ParachainBlockImport<RuntimeApi>,
	prometheus_registry: Option<&Registry>,
	telemetry: Option<TelemetryHandle>,
	task_manager: &TaskManager,
	relay_chain_interface: Arc<dyn RelayChainInterface>,
	transaction_pool: Arc<sc_transaction_pool::FullPool<Block, ParachainClient<RuntimeApi>>>,
	keystore: KeystorePtr,
	relay_chain_slot_duration: Duration,
	para_id: ParaId,
	collator_key: CollatorPair,
	overseer_handle: OverseerHandle,
	announce_block: Arc<dyn Fn(Hash, Option<Vec<u8>>) + Send + Sync>,
	backend: Arc<ParachainBackend>,
) -> Result<(), sc_service::Error>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
		task_manager.spawn_handle(),
		client.clone(),
		transaction_pool,
		prometheus_registry,
		telemetry.clone(),
	);

	let collator_service = CollatorService::new(
		client.clone(),
		Arc::new(task_manager.spawn_handle()),
		announce_block,
		client.clone(),
	);

	let params = AuraParams {
		create_inherent_data_providers: move |_, ()| async move { Ok(()) },
		block_import,
		para_client: client.clone(),
		para_backend: backend,
		relay_client: relay_chain_interface,
		code_hash_provider: {
			let client = client.clone();
			move |block_hash| {
				client.code_at(block_hash).ok().map(|c| ValidationCode::from(c).hash())
			}
		},
		keystore,
		collator_key,
		para_id,
		overseer_handle,
		relay_chain_slot_duration,
		proposer: Proposer::new(proposer_factory),
		collator_service,
		authoring_duration: Duration::from_millis(1500),
		reinitialize: false,
	};

	let fut = async move {
		wait_for_aura(client).await;
		aura::run::<Block, <AuraId as AppCrypto>::Pair, _, _, _, _, _, _, _, _>(params).await;
	};
	task_manager.spawn_essential_handle().spawn("aura", None, fut);

	Ok(())
}

/// Start consensus using the lookahead aura collator.
fn start_slot_based_aura_consensus<RuntimeApi, AuraId>(
	client: Arc<ParachainClient<RuntimeApi>>,
	block_import: ParachainBlockImport<RuntimeApi>,
	prometheus_registry: Option<&Registry>,
	telemetry: Option<TelemetryHandle>,
	task_manager: &TaskManager,
	relay_chain_interface: Arc<dyn RelayChainInterface>,
	transaction_pool: Arc<sc_transaction_pool::FullPool<Block, ParachainClient<RuntimeApi>>>,
	keystore: KeystorePtr,
	relay_chain_slot_duration: Duration,
	para_id: ParaId,
	collator_key: CollatorPair,
	_overseer_handle: OverseerHandle,
	announce_block: Arc<dyn Fn(Hash, Option<Vec<u8>>) + Send + Sync>,
	backend: Arc<ParachainBackend>,
) -> Result<(), sc_service::Error>
where
	RuntimeApi: ConstructNodeRuntimeApi<Block, ParachainClient<RuntimeApi>>,
	RuntimeApi::RuntimeApi: AuraRuntimeApi<Block, AuraId>,
	AuraId: AuraIdT + Sync,
{
	let proposer_factory = sc_basic_authorship::ProposerFactory::with_proof_recording(
		task_manager.spawn_handle(),
		client.clone(),
		transaction_pool,
		prometheus_registry,
		telemetry.clone(),
	);

	let proposer = Proposer::new(proposer_factory);
	let collator_service = CollatorService::new(
		client.clone(),
		Arc::new(task_manager.spawn_handle()),
		announce_block,
		client.clone(),
	);

	let client_for_aura = client.clone();
	let params = SlotBasedParams {
		create_inherent_data_providers: move |_, ()| async move { Ok(()) },
		block_import,
		para_client: client.clone(),
		para_backend: backend.clone(),
		relay_client: relay_chain_interface,
		code_hash_provider: move |block_hash| {
			client_for_aura.code_at(block_hash).ok().map(|c| ValidationCode::from(c).hash())
		},
		keystore,
		collator_key,
		para_id,
		relay_chain_slot_duration,
		proposer,
		collator_service,
		authoring_duration: Duration::from_millis(2000),
		reinitialize: false,
		slot_drift: Duration::from_secs(1),
	};

	let (collation_future, block_builder_future) =
		slot_based::run::<Block, <AuraId as AppCrypto>::Pair, _, _, _, _, _, _, _, _>(params);

	task_manager.spawn_essential_handle().spawn(
		"collation-task",
		Some("parachain-block-authoring"),
		collation_future,
	);
	task_manager.spawn_essential_handle().spawn(
		"block-builder-task",
		Some("parachain-block-authoring"),
		block_builder_future,
	);
	Ok(())
}

/// Start an aura powered parachain node which uses the lookahead collator to support async backing.
/// This node is basic in the sense that its runtime api doesn't include common contents such as
/// transaction payment. Used for aura glutton.
pub async fn start_basic_async_backing_node<Net: NetworkBackend<Block, Hash>>(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	para_id: ParaId,
	use_experimental_slot_based: bool,
	hwbench: Option<sc_sysinfo::HwBench>,
) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient<FakeRuntimeApi>>)> {
	let consensus_starter = if use_experimental_slot_based {
		start_slot_based_aura_consensus::<_, AuraId>
	} else {
		start_lookahead_aura_consensus::<_, AuraId>
	};
	start_node_impl::<FakeRuntimeApi, _, _, _, Net>(
		parachain_config,
		polkadot_config,
		collator_options,
		CollatorSybilResistance::Resistant, // Aura
		para_id,
		|_, _, _, _| Ok(RpcModule::new(())),
		build_relay_to_aura_import_queue::<_, AuraId>,
		consensus_starter,
		hwbench,
	)
	.await
}

/// Start a parachain node for Rococo Contracts.
pub async fn start_contracts_rococo_node<Net: NetworkBackend<Block, Hash>>(
	parachain_config: Configuration,
	polkadot_config: Configuration,
	collator_options: CollatorOptions,
	para_id: ParaId,
	use_experimental_slot_based: bool,
	hwbench: Option<sc_sysinfo::HwBench>,
) -> sc_service::error::Result<(TaskManager, Arc<ParachainClient<FakeRuntimeApi>>)> {
	let consensus_starter = if use_experimental_slot_based {
		start_slot_based_aura_consensus::<_, AuraId>
	} else {
		start_lookahead_aura_consensus::<_, AuraId>
	};
	start_node_impl::<FakeRuntimeApi, _, _, _, Net>(
		parachain_config,
		polkadot_config,
		collator_options,
		CollatorSybilResistance::Resistant, // Aura
		para_id,
		build_contracts_rpc_extensions,
		build_aura_import_queue,
		consensus_starter,
		hwbench,
	)
	.await
}

/// Checks that the hardware meets the requirements and print a warning otherwise.
fn warn_if_slow_hardware(hwbench: &sc_sysinfo::HwBench) {
	// Polkadot para-chains should generally use these requirements to ensure that the relay-chain
	// will not take longer than expected to import its blocks.
	if let Err(err) = frame_benchmarking_cli::SUBSTRATE_REFERENCE_HARDWARE.check_hardware(hwbench) {
		log::warn!(
			"⚠️  The hardware does not meet the minimal requirements {} for role 'Authority' find out more at:\n\
			https://wiki.polkadot.network/docs/maintain-guides-how-to-validate-polkadot#reference-hardware",
			err
		);
	}
}
