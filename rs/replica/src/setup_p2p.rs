use ic_btc_adapter_client::{setup_bitcoin_adapter_clients, BitcoinAdapterClients};
use ic_btc_consensus::BitcoinPayloadBuilder;
use ic_config::{artifact_pool::ArtifactPoolConfig, subnet_config::SubnetConfig, Config};
use ic_consensus::certification::VerifierImpl;
use ic_crypto::CryptoComponent;
use ic_cycles_account_manager::CyclesAccountManager;
use ic_execution_environment::ExecutionServices;
use ic_interfaces::execution_environment::AnonymousQueryService;
use ic_interfaces::{
    certified_stream_store::CertifiedStreamStore,
    consensus_pool::ConsensusPoolCache,
    execution_environment::{IngressFilterService, QueryExecutionService, QueryHandler},
    registry::{LocalStoreCertifiedTimeReader, RegistryClient},
};
use ic_interfaces_p2p::IngressIngestionService;
use ic_logger::{info, ReplicaLogger};
use ic_messaging::MessageRoutingImpl;
use ic_p2p::P2PThreadJoiner;
use ic_registry_subnet_type::SubnetType;
use ic_replica_setup_ic_network::{
    create_networking_stack, init_artifact_pools, P2PStateSyncClient,
};
use ic_replicated_state::ReplicatedState;
use ic_state_manager::StateManagerImpl;
use ic_types::{consensus::catchup::CUPWithOriginalProtobuf, NodeId, SubnetId};
use ic_xnet_endpoint::{XNetEndpoint, XNetEndpointConfig};
use ic_xnet_payload_builder::XNetPayloadBuilderImpl;
use std::sync::Arc;

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub fn construct_ic_stack(
    replica_logger: ReplicaLogger,
    rt_handle: tokio::runtime::Handle,
    rt_handle_xnet: tokio::runtime::Handle,
    config: Config,
    subnet_config: SubnetConfig,
    node_id: NodeId,
    subnet_id: SubnetId,
    subnet_type: SubnetType,
    registry: Arc<dyn RegistryClient + Send + Sync>,
    crypto: Arc<CryptoComponent>,
    metrics_registry: ic_metrics::MetricsRegistry,
    catch_up_package: Option<CUPWithOriginalProtobuf>,
    local_store_time_reader: Option<Arc<dyn LocalStoreCertifiedTimeReader>>,
) -> std::io::Result<(
    // TODO(SCL-213): When Rust traits support it, simplify and pass a single
    // trait.
    Arc<CryptoComponent>,
    Arc<StateManagerImpl>,
    Arc<dyn QueryHandler<State = ReplicatedState>>,
    QueryExecutionService,
    AnonymousQueryService,
    P2PThreadJoiner,
    IngressIngestionService,
    Arc<dyn ConsensusPoolCache>,
    IngressFilterService,
    XNetEndpoint,
)> {
    let artifact_pool_config = ArtifactPoolConfig::from(config.artifact_pool);

    // Determine the correct catch-up package.
    let catch_up_package = {
        use ic_types::consensus::HasHeight;
        let make_registry_cup = || {
            CUPWithOriginalProtobuf::from_cup(
                ic_consensus::dkg::make_registry_cup(&*registry, subnet_id, None)
                    .expect("Couldn't create a registry CUP"),
            )
        };
        match catch_up_package {
            // The orchestrator has persisted a CUP for the replica.
            Some(cup_from_nm) => {
                let signed = !cup_from_nm
                    .cup
                    .signature
                    .signature
                    .clone()
                    .get()
                    .0
                    .is_empty();
                if signed {
                    // The CUP persisted by the orchestrator is safe to use because it's signed.
                    info!(
                        &replica_logger,
                        "Using the signed CUP with height {}",
                        cup_from_nm.cup.height()
                    );
                    cup_from_nm
                } else {
                    // The CUP persisted by the orchestrator is unsigned and hence it was created
                    // from the registry CUP contents. In this case, we re-create this CUP to avoid
                    // incompatibility issues, because on other replicas of the same subnet the node
                    // manager version may differ, so the CUP contents might differ as well.
                    let registry_cup = make_registry_cup();
                    // However in a special case of the NNS subnet recovery, we still have to use
                    // a newer unsigned CUP. The incompatibility issue is not a problem in this
                    // case, because this CUP will not be created by the orchestrator.
                    if registry_cup.cup.height() < cup_from_nm.cup.height() {
                        info!(
                            &replica_logger,
                            "Using the newer CUP with height {} passed from the orchestrator",
                            cup_from_nm.cup.height()
                        );
                        cup_from_nm
                    } else {
                        info!(
                            &replica_logger,
                            "Using the CUP with height {} generated from the registry (CUP height from the orchestrator is {})",
                            registry_cup.cup.height(),
                            cup_from_nm.cup.height()
                        );
                        registry_cup
                    }
                }
            }
            // No CUP was persisted by the orchestrator, which is usually the case for fresh nodes.
            None => {
                let registry_cup = make_registry_cup();
                info!(
                    &replica_logger,
                    "Using the CUP with height {} generated from the registry",
                    registry_cup.cup.height()
                );
                registry_cup
            }
        }
    };

    let artifact_pools = init_artifact_pools(
        subnet_id,
        artifact_pool_config,
        metrics_registry.clone(),
        replica_logger.clone(),
        catch_up_package,
    );

    let cycles_account_manager = Arc::new(CyclesAccountManager::new(
        subnet_config.scheduler_config.max_instructions_per_message,
        subnet_type,
        subnet_id,
        subnet_config.cycles_account_manager_config,
    ));
    let verifier = VerifierImpl::new(crypto.clone());
    let state_manager = Arc::new(StateManagerImpl::new(
        Arc::new(verifier),
        subnet_id,
        subnet_type,
        replica_logger.clone(),
        &metrics_registry,
        &config.state_manager,
        Some(artifact_pools.consensus_pool_cache.starting_height()),
        config.malicious_behaviour.malicious_flags.clone(),
    ));
    let execution_services = ExecutionServices::setup_execution(
        replica_logger.clone(),
        &metrics_registry,
        subnet_id,
        subnet_type,
        subnet_config.scheduler_config,
        config.hypervisor.clone(),
        Arc::clone(&cycles_account_manager),
        Arc::clone(&state_manager) as Arc<_>,
    );

    let certified_stream_store: Arc<dyn CertifiedStreamStore> =
        Arc::clone(&state_manager) as Arc<_>;

    let message_router = if config
        .malicious_behaviour
        .malicious_flags
        .maliciously_disable_execution
    {
        MessageRoutingImpl::new_fake(
            subnet_id,
            Arc::clone(&state_manager) as Arc<_>,
            execution_services.ingress_history_writer,
            &metrics_registry,
            replica_logger.clone(),
        )
    } else {
        MessageRoutingImpl::new(
            Arc::clone(&state_manager) as Arc<_>,
            Arc::clone(&certified_stream_store) as Arc<_>,
            execution_services.ingress_history_writer,
            execution_services.scheduler,
            config.hypervisor,
            Arc::clone(&cycles_account_manager),
            subnet_id,
            &metrics_registry,
            replica_logger.clone(),
            Arc::clone(&registry) as Arc<_>,
        )
    };
    let message_router = Arc::new(message_router);

    let xnet_config =
        XNetEndpointConfig::from(Arc::clone(&registry) as Arc<_>, node_id, &replica_logger);

    let xnet_endpoint = XNetEndpoint::new(
        rt_handle_xnet,
        Arc::clone(&certified_stream_store),
        Arc::clone(&crypto) as Arc<_>,
        Arc::clone(&registry),
        xnet_config,
        &metrics_registry,
        replica_logger.clone(),
    );

    // Use default runtime to spawn xnet client threads.
    let xnet_payload_builder = XNetPayloadBuilderImpl::new(
        Arc::clone(&state_manager) as Arc<_>,
        Arc::clone(&certified_stream_store) as Arc<_>,
        Arc::clone(&crypto) as Arc<_>,
        Arc::clone(&registry) as Arc<_>,
        rt_handle.clone(),
        node_id,
        subnet_id,
        &metrics_registry,
        replica_logger.clone(),
    );
    let xnet_payload_builder = Arc::new(xnet_payload_builder);

    let BitcoinAdapterClients {
        btc_testnet_client,
        btc_mainnet_client,
    } = setup_bitcoin_adapter_clients(
        replica_logger.clone(),
        &metrics_registry,
        rt_handle.clone(),
        config.adapters_config.clone(),
    );
    let self_validating_payload_builder = BitcoinPayloadBuilder::new(
        state_manager.clone(),
        &metrics_registry,
        btc_mainnet_client,
        btc_testnet_client,
        subnet_id,
        Arc::clone(&registry),
        replica_logger.clone(),
    );
    let self_validating_payload_builder = Arc::new(self_validating_payload_builder);

    let (ingress_ingestion_service, p2p_runner) = create_networking_stack(
        metrics_registry,
        replica_logger,
        rt_handle,
        config.transport,
        config.consensus,
        config.malicious_behaviour.malicious_flags,
        node_id,
        subnet_id,
        None,
        Arc::clone(&crypto) as Arc<_>,
        Arc::clone(&state_manager) as Arc<_>,
        P2PStateSyncClient::Client(Arc::clone(&state_manager) as Arc<_>),
        xnet_payload_builder as Arc<_>,
        self_validating_payload_builder as Arc<_>,
        message_router as Arc<_>,
        // TODO(SCL-213)
        Arc::clone(&crypto) as Arc<_>,
        Arc::clone(&crypto) as Arc<_>,
        Arc::clone(&crypto) as Arc<_>,
        Arc::clone(&crypto) as Arc<_>,
        registry,
        execution_services.ingress_history_reader,
        &artifact_pools,
        cycles_account_manager,
        local_store_time_reader,
        config.nns_registry_replicator.poll_delay_duration_ms,
    );
    Ok((
        crypto,
        state_manager,
        execution_services.sync_query_handler,
        execution_services.async_query_handler,
        execution_services.anonymous_query_handler,
        p2p_runner,
        ingress_ingestion_service,
        artifact_pools.consensus_pool_cache,
        execution_services.ingress_filter,
        xnet_endpoint,
    ))
}
