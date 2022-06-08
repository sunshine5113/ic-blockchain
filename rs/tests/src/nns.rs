//! Contains methods and structs that support settings up the NNS.
use crate::{
    driver::test_env_api::install_nns_canisters,
    util::{block_on, create_agent, runtime_from_url},
};
use candid::CandidType;
use canister_test::{Canister, Runtime};
use cycles_minting_canister::SetAuthorizedSubnetworkListArgs;
use dfn_candid::candid_one;
use ic_base_types::NodeId;
use ic_canister_client::Sender;
use ic_config::subnet_config::SchedulerConfig;
use ic_fondue::{
    ic_instance::node_software_version::NodeSoftwareVersion,
    ic_manager::{IcEndpoint, IcHandle},
};
use ic_nervous_system_common_test_keys::TEST_NEURON_1_OWNER_KEYPAIR;
use ic_nns_common::types::{NeuronId, ProposalId};
use ic_nns_constants::GOVERNANCE_CANISTER_ID;
use ic_nns_governance::pb::v1::{
    manage_neuron::{Command, NeuronIdOrSubaccount, RegisterVote},
    ManageNeuron, ManageNeuronResponse, NnsFunction, ProposalInfo, ProposalStatus, Vote,
};
use ic_nns_test_utils::governance::submit_external_update_proposal_allowing_error;
use ic_nns_test_utils::governance::{submit_external_update_proposal, wait_for_final_state};
use ic_nns_test_utils::{governance::get_proposal_info, ids::TEST_NEURON_1_ID};
use ic_prep_lib::subnet_configuration::{self, duration_to_millis};
use ic_protobuf::registry::subnet::v1::SubnetListRecord;
use ic_registry_client_helpers::deserialize_registry_value;
use ic_registry_keys::{get_node_record_node_id, make_subnet_list_record_key};
use ic_registry_local_store::{LocalStoreImpl, LocalStoreReader};
use ic_registry_nns_data_provider::registry::RegistryCanister;
use ic_registry_subnet_features::SubnetFeatures;
use ic_registry_subnet_type::SubnetType;
use ic_types::{p2p, CanisterId, PrincipalId, RegistryVersion, ReplicaVersion, SubnetId};
use prost::Message;
use registry_canister::mutations::{
    do_add_nodes_to_subnet::AddNodesToSubnetPayload, do_create_subnet::CreateSubnetPayload,
    do_remove_nodes_from_subnet::RemoveNodesFromSubnetPayload,
    do_update_unassigned_nodes_config::UpdateUnassignedNodesConfigPayload,
};
use registry_canister::mutations::{
    do_bless_replica_version::BlessReplicaVersionPayload,
    do_update_subnet_replica::UpdateSubnetReplicaVersionPayload,
};
use slog::info;
use std::convert::TryFrom;
use std::time::Duration;
use tokio::time::sleep;
use url::Url;

/// Installation of NNS Canisters.

pub trait NnsExt {
    fn install_nns_canisters(&self, handle: &IcHandle, nns_test_neurons_present: bool);

    /// Convenience method to bless a software update using the binaries
    /// available on the $PATH.
    ///
    /// Generates a new `ReplicaVersionRecord` with replica version `version`.
    /// Depending on `package_content`, only `orchestrator`, only `replica`, or
    /// both, will be updated with the given version. The binaries that are
    /// referenced in the update are the same that are used as the initial
    /// replica version.
    ///
    /// This function can only succeed if the NNS with test neurons have been
    /// installed on the root subnet.
    fn bless_replica_version(
        &self,
        handle: &IcHandle,
        node_implementation_version: NodeSoftwareVersion,
        package_content: UpgradeContent,
    );

    /// Update the subnet given by the subnet index `subnet_index` (enumerated
    /// in order in which they were added) to version `version`.
    ///
    /// This function can only succeed if the NNS with test neurons have been
    /// installed on the root subnet.
    ///
    /// # Panics
    ///
    /// This function will panic if the index is out of bounds wrt. to the
    /// subnets that were _initially_ added to the IC; subnets that were added
    /// after bootstrapping the IC are not supported.
    fn update_subnet_by_idx(&self, handle: &IcHandle, subnet_index: usize, version: ReplicaVersion);

    /// Waits for a given software version `version` to become available on the
    /// subnet with subnet index `subnet_index`.
    ///
    /// This method assumes that only one application subnet is present and that
    /// that subnet is being updated.
    fn await_status_change(
        &self,
        endpoint: &IcEndpoint,
        retry_delay: Duration,
        timeout: Duration,
        acceptance_criterium: impl Fn(&ic_agent::agent::status::Status) -> bool,
    ) -> bool;

    /// Removes nodes from their subnet.
    fn remove_nodes(&self, handle: &IcHandle, node_ids: &[NodeId]);

    /// A list of all nodes that were registered with the initial registry (i.e.
    /// at bootstrap).
    fn initial_node_ids(&self, handle: &IcHandle) -> Vec<NodeId> {
        let ic_prep_dir = handle
            .ic_prep_working_dir
            .as_ref()
            .expect("ic_prep_working_dir is not set.");

        LocalStoreImpl::new(ic_prep_dir.registry_local_store_path().as_path())
            .get_changelog_since_version(RegistryVersion::from(0))
            .expect("Could not fetch changelog.")
            .iter()
            .flat_map(|c| c.iter())
            .filter_map(|km| {
                km.value
                    .as_ref()
                    .map(|_| &km.key)
                    .and_then(|s| get_node_record_node_id(s))
            })
            .map(NodeId::from)
            .collect()
    }

    fn initial_unassigned_node_endpoints(&self, handle: &IcHandle) -> Vec<IcEndpoint> {
        handle
            .public_api_endpoints
            .iter()
            .filter(|ep| ep.subnet.is_none())
            .cloned()
            .collect::<Vec<IcEndpoint>>()
    }
}

impl NnsExt for ic_fondue::pot::Context {
    fn install_nns_canisters(&self, handle: &IcHandle, nns_test_neurons_present: bool) {
        let mut is_installed = self.is_nns_installed.lock().unwrap();
        let endpoint = first_root_endpoint(handle);
        block_on(async move {
            endpoint.assert_ready(self).await;
        });
        if is_installed.eq(&false) {
            install_nns_canisters(
                &self.logger,
                endpoint.url.clone(),
                handle.ic_prep_working_dir.as_ref().unwrap(),
                nns_test_neurons_present,
            );
            *is_installed = true;
        }
    }

    fn await_status_change(
        &self,
        endpoint: &IcEndpoint,
        retry_delay: Duration,
        timeout: Duration,
        acceptance_criterium: impl Fn(&ic_agent::agent::status::Status) -> bool,
    ) -> bool {
        block_on(async move {
            endpoint.assert_ready(self).await;
            await_replica_status_change(self, endpoint, retry_delay, timeout, acceptance_criterium)
                .await
        })
    }

    fn bless_replica_version(
        &self,
        handle: &IcHandle,
        impl_version: NodeSoftwareVersion,
        _package_content: UpgradeContent,
    ) {
        let replica_version = impl_version.replica_version;
        let root_url = first_root_url(handle);
        block_on(async move {
            let rt = runtime_from_url(root_url);
            add_replica_version(&rt, replica_version)
                .await
                .expect("adding replica version failed.");
        });
    }

    fn update_subnet_by_idx(
        &self,
        handle: &IcHandle,
        subnet_index: usize,
        version: ReplicaVersion,
    ) {
        // get the subnet id of the subnet with index subnet index
        let reg_path = handle
            .ic_prep_working_dir
            .as_ref()
            .unwrap()
            .registry_local_store_path();
        let local_store = LocalStoreImpl::new(&reg_path);
        let changelog = local_store
            .get_changelog_since_version(RegistryVersion::from(0))
            .expect("Could not read registry.");

        // The initial registry may only contain a single version.
        let bytes = changelog
            .first()
            .expect("Empty changelog")
            .iter()
            .find_map(|k| {
                if k.key == make_subnet_list_record_key() {
                    Some(k.value.clone().expect("Subnet list not set"))
                } else {
                    None
                }
            })
            .expect("Subnet list not found");
        let subnet_list_record =
            SubnetListRecord::decode(&bytes[..]).expect("Could not decode subnet list record.");
        let subnet_id = SubnetId::from(
            PrincipalId::try_from(&subnet_list_record.subnets[subnet_index][..]).unwrap(),
        );

        let url = first_root_url(handle);
        // send the update proposal
        block_on(async move {
            let rt = runtime_from_url(url);
            update_subnet_replica_version(&rt, subnet_id, version.to_string())
                .await
                .expect("updating subnet failed");
        });
    }

    fn remove_nodes(&self, handle: &IcHandle, node_ids: &[NodeId]) {
        let rt = tokio::runtime::Runtime::new().expect("Tokio runtime failed to create");
        rt.block_on(async move {
            remove_nodes(handle, node_ids).await.unwrap();
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UpgradeContent {
    All,
    Orchestrator,
    Replica,
}

pub fn first_root_url(ic_handle: &IcHandle) -> Url {
    first_root_endpoint(ic_handle).url.clone()
}

pub fn first_root_endpoint(ic_handle: &IcHandle) -> &IcEndpoint {
    ic_handle
        .public_api_endpoints
        .iter()
        .find(|i| i.is_root_subnet)
        .expect("empty iterator")
}

/// Send an update-call to the governance-canister on the NNS asking for Subnet
/// `subnet_id` to be updated to replica with version id `replica_version_id`.
async fn update_subnet_replica_version(
    nns_api: &'_ Runtime,
    subnet_id: SubnetId,
    replica_version_id: String,
) -> Result<(), String> {
    let governance_canister = get_governance_canister(nns_api);
    let proposal_payload = UpdateSubnetReplicaVersionPayload {
        subnet_id: subnet_id.get(),
        replica_version_id,
    };

    let proposal_id = submit_external_proposal_with_test_id(
        &governance_canister,
        NnsFunction::UpdateSubnetReplicaVersion,
        proposal_payload,
    )
    .await;

    vote_execute_proposal_assert_executed(&governance_canister, proposal_id).await;
    Ok(())
}

/// Detect whether a proposal is executed within `timeout`.
///
/// # Arguments
///
/// * `ctx`         - Fondue context
/// * `governance`  - Governance canister
/// * `proposal_id` - ID of a proposal to be executed
/// * `retry_delay` - Duration between polling attempts
/// * `timeout`     - Duration after which we give up (returning false)
///
/// Eventually returns whether the proposal has been executed.
pub async fn await_proposal_execution(
    ctx: &ic_fondue::pot::Context,
    governance: &Canister<'_>,
    proposal_id: ProposalId,
    retry_delay: Duration,
    timeout: Duration,
) -> bool {
    let mut i = 0usize;
    let start_time = std::time::Instant::now();
    loop {
        i += 1;
        info!(
            ctx.logger,
            "Attempt #{} of obtaining final execution status for {:?}", i, proposal_id
        );

        let proposal_info = get_proposal_info(governance, proposal_id)
            .await
            .unwrap_or_else(|| panic!("could not obtain proposal status"));

        match ProposalStatus::from_i32(proposal_info.status).unwrap() {
            ProposalStatus::Open => {
                // This proposal is still open
                info!(ctx.logger, "{:?} is open...", proposal_id,)
            }
            ProposalStatus::Adopted => {
                // This proposal is adopted but not yet executed
                info!(ctx.logger, "{:?} is adopted...", proposal_id,)
            }
            ProposalStatus::Executed => {
                // This proposal is already executed
                info!(ctx.logger, "{:?} has been executed.", proposal_id,);
                return true;
            }
            other_status => {
                // This proposal will not be executed
                info!(
                    ctx.logger,
                    "{:?} could not be adopted: {:?}", proposal_id, other_status
                );
                return false;
            }
        }

        if std::time::Instant::now()
            .duration_since(start_time)
            .gt(&timeout)
        {
            // Give up
            return false;
        } else {
            // Continue polling with delay
            sleep(retry_delay).await;
        }
    }
}

/// Detect whether a replica's status becomes acceptable within `timeout`.
///
/// # Arguments
///
/// * `ctx`                  - Fondue context
/// * `endpoint`             - Endpoint of a subnet
/// * `retry_delay`          - Duration between polling attempts
/// * `timeout`              - Duration after which we give up (returning false)
/// * `acceptance_criterium` - Predicate determining whether the current status
///   is accepted
///
/// Eventually returns whether the replica status has changed as specified via
/// `acceptance_criterium`.
pub async fn await_replica_status_change(
    ctx: &ic_fondue::pot::Context,
    endpoint: &IcEndpoint,
    retry_delay: Duration,
    timeout: Duration,
    acceptance_criterium: impl Fn(&ic_agent::agent::status::Status) -> bool,
) -> bool {
    let start_time = std::time::Instant::now();
    let mut i = 0usize;
    loop {
        i += 1;
        info!(
            ctx.logger,
            "Attempt #{} of detecting replica status change", i
        );

        let status = get_replica_status(endpoint)
            .await
            .expect("Could not obtain new agent status");

        if acceptance_criterium(&status) {
            info!(
                ctx.logger,
                " status change has been accepted.\nNew status:\n{:?}", status
            );
            return true;
        }

        if std::time::Instant::now()
            .duration_since(start_time)
            .gt(&timeout)
        {
            // Give up
            info!(
                ctx.logger,
                " did not detect status change within {:?}.\nStatus remains:\n{:?}",
                timeout,
                status
            );
            return false;
        } else {
            // Continue polling with delay
            sleep(retry_delay).await;
        }
    }
}

/// Obtain the status of a replica via its `endpoint`.
///
/// Eventually returns the status of the replica.
async fn get_replica_status(
    endpoint: &IcEndpoint,
) -> Result<ic_agent::agent::status::Status, ic_agent::AgentError> {
    match create_agent(&endpoint.url.to_string()).await {
        Ok(agent) => agent.status().await,
        Err(e) => Err(e),
    }
}

/// Obtain the software version of a replica via its `endpoint`.
///
/// Eventually returns the replica software version.
pub async fn get_software_version(endpoint: &IcEndpoint) -> Option<ReplicaVersion> {
    match get_replica_status(endpoint).await {
        Ok(status) => status
            .impl_version
            .map(|v| ReplicaVersion::try_from(v).unwrap()),
        Err(_) => None,
    }
}

/// Adds the given `ReplicaVersionRecord` to the registry and returns the
/// registry version after the update.
async fn add_replica_version(nns_api: &'_ Runtime, version: ReplicaVersion) -> Result<(), String> {
    let governance_canister = get_governance_canister(nns_api);
    let proposal_payload = BlessReplicaVersionPayload {
        replica_version_id: version.to_string(),
        binary_url: "".into(),
        sha256_hex: "".into(),
        node_manager_binary_url: "".into(),
        node_manager_sha256_hex: "".into(),
        release_package_url: "".to_string(),
        release_package_sha256_hex: "".to_string(),
    };

    let proposal_id: ProposalId = submit_external_proposal_with_test_id(
        &governance_canister,
        NnsFunction::BlessReplicaVersion,
        proposal_payload,
    )
    .await;

    vote_execute_proposal_assert_executed(&governance_canister, proposal_id).await;

    Ok(())
}

pub async fn update_xdr_per_icp(
    nns_api: &'_ Runtime,
    timestamp_seconds: u64,
    xdr_permyriad_per_icp: u64,
) -> Result<(), String> {
    let governance_canister = get_governance_canister(nns_api);
    let proposal_payload = ic_nns_common::types::UpdateIcpXdrConversionRatePayload {
        data_source: "".to_string(),
        timestamp_seconds,
        xdr_permyriad_per_icp,
    };

    let proposal_id = submit_external_proposal_with_test_id(
        &governance_canister,
        NnsFunction::IcpXdrConversionRate,
        proposal_payload,
    )
    .await;

    vote_execute_proposal_assert_executed(&governance_canister, proposal_id).await;
    Ok(())
}

pub async fn set_authorized_subnetwork_list(
    nns_api: &'_ Runtime,
    who: Option<PrincipalId>,
    subnets: Vec<SubnetId>,
) -> Result<(), String> {
    let governance_canister = get_governance_canister(nns_api);
    let proposal_payload = SetAuthorizedSubnetworkListArgs { who, subnets };

    let proposal_id = submit_external_proposal_with_test_id(
        &governance_canister,
        NnsFunction::SetAuthorizedSubnetworks,
        proposal_payload,
    )
    .await;

    vote_execute_proposal_assert_executed(&governance_canister, proposal_id).await;
    Ok(())
}

async fn remove_nodes(handle: &IcHandle, node_ids: &[NodeId]) -> Result<(), String> {
    let root_url = first_root_url(handle);
    remove_nodes_via_endpoint(root_url, node_ids).await
}

pub async fn add_nodes_to_subnet(
    url: Url,
    subnet_id: SubnetId,
    node_ids: &[NodeId],
) -> Result<(), String> {
    let nns_api = runtime_from_url(url);
    let governance_canister = get_canister(&nns_api, GOVERNANCE_CANISTER_ID);
    let proposal_payload = AddNodesToSubnetPayload {
        node_ids: node_ids.to_vec(),
        subnet_id: subnet_id.get(),
    };

    let proposal_id = submit_external_update_proposal(
        &governance_canister,
        Sender::from_keypair(&TEST_NEURON_1_OWNER_KEYPAIR),
        NeuronId(TEST_NEURON_1_ID),
        NnsFunction::AddNodeToSubnet,
        proposal_payload,
        String::from("Add nodes for testing"),
        "".to_string(),
    )
    .await;

    vote_and_execute_proposal(&governance_canister, proposal_id).await;
    Ok(())
}

pub async fn remove_nodes_via_endpoint(url: Url, node_ids: &[NodeId]) -> Result<(), String> {
    let nns_api = runtime_from_url(url);
    let governance_canister = get_canister(&nns_api, GOVERNANCE_CANISTER_ID);
    let proposal_payload = RemoveNodesFromSubnetPayload {
        node_ids: node_ids.to_vec(),
    };

    let proposal_id = submit_external_update_proposal(
        &governance_canister,
        Sender::from_keypair(&TEST_NEURON_1_OWNER_KEYPAIR),
        NeuronId(TEST_NEURON_1_ID),
        NnsFunction::RemoveNodesFromSubnet,
        proposal_payload,
        String::from("Remove node for testing"),
        "".to_string(),
    )
    .await;

    vote_and_execute_proposal(&governance_canister, proposal_id).await;
    Ok(())
}

pub fn get_canister(nns_api: &'_ Runtime, canister_id: CanisterId) -> Canister<'_> {
    Canister::new(nns_api, canister_id)
}

/// Votes for and executes the proposal identified by `proposal_id`. Asserts
/// that the ProposalStatus is Executed.
pub async fn vote_execute_proposal_assert_executed(
    governance_canister: &Canister<'_>,
    proposal_id: ProposalId,
) {
    // Wait for the proposal to be accepted and executed.
    assert_eq!(
        vote_and_execute_proposal(governance_canister, proposal_id)
            .await
            .status(),
        ProposalStatus::Executed
    );
}

/// Votes for and executes the proposal identified by `proposal_id`. Asserts
/// that the ProposalStatus is Failed.
///
/// It is also verified that the rejection message contains (case-insensitive)
/// expected_message_substring. This can be left empty to guarantee a match when
/// not needed.
pub async fn vote_execute_proposal_assert_failed(
    governance_canister: &Canister<'_>,
    proposal_id: ProposalId,
    expected_message_substring: impl ToString,
) {
    let expected_message_substring = expected_message_substring.to_string();
    // Wait for the proposal to be accepted and executed.
    let proposal_info = vote_and_execute_proposal(governance_canister, proposal_id).await;
    assert_eq!(proposal_info.status(), ProposalStatus::Failed);
    let reason = proposal_info.failure_reason.unwrap_or_default();
    assert!(
       reason
            .error_message
            .to_lowercase()
            .contains(expected_message_substring.to_lowercase().as_str()),
        "Rejection error for proposal {}, which is '{}', does not contain the expected substring '{}'",
        proposal_id,
        reason,
        expected_message_substring
    );
}

pub async fn vote_and_execute_proposal(
    governance_canister: &Canister<'_>,
    proposal_id: ProposalId,
) -> ProposalInfo {
    // Cast votes.
    let input = ManageNeuron {
        neuron_id_or_subaccount: Some(NeuronIdOrSubaccount::NeuronId(
            ic_nns_common::pb::v1::NeuronId {
                id: TEST_NEURON_1_ID,
            },
        )),
        id: None,
        command: Some(Command::RegisterVote(RegisterVote {
            vote: Vote::Yes as i32,
            proposal: Some(ic_nns_common::pb::v1::ProposalId { id: proposal_id.0 }),
        })),
    };
    let _result: ManageNeuronResponse = governance_canister
        .update_from_sender(
            "manage_neuron",
            candid_one,
            input,
            &Sender::from_keypair(&TEST_NEURON_1_OWNER_KEYPAIR),
        )
        .await
        .expect("Vote failed");
    wait_for_final_state(governance_canister, proposal_id).await
}

pub fn get_governance_canister(nns_api: &'_ Runtime) -> Canister<'_> {
    get_canister(nns_api, GOVERNANCE_CANISTER_ID)
}

pub async fn submit_external_proposal_with_test_id<T: CandidType>(
    governance_canister: &Canister<'_>,
    nns_function: NnsFunction,
    payload: T,
) -> ProposalId {
    let sender = Sender::from_keypair(&TEST_NEURON_1_OWNER_KEYPAIR);
    let neuron_id = NeuronId(TEST_NEURON_1_ID);
    submit_external_update_proposal(
        governance_canister,
        sender,
        neuron_id,
        nns_function,
        payload,
        "<proposal created by submit_external_proposal_with_test_id>".to_string(),
        "".to_string(),
    )
    .await
}

/// Submits a proposal for blessing a replica software version.
///
/// # Arguments
///
/// * `governance`  - Governance canister
/// * `sender`      - Sender of the proposal
/// * `neuron_id`   - ID of the proposing neuron. This neuron will automatically
///   vote in favor of the proposal.
/// * `version`     - Replica software version
/// * `sha256`      - Claimed SHA256 of the replica image file
/// * `upgrade_url` - URL leading to the replica image file
///
/// Note: The existing replica *may or may not* check that the
/// provided `sha256` corresponds to the image checksum. In case
/// this proposal is adopted, the replica *assumes* that the file
/// under `upgrade_url` has the provided `sha256`. If there has
/// been a mismatch (or if the image has been forged after blessing),
/// the replica will reject the follow-up proposal for updating the
/// replica version.
///
/// Eventually returns the identifier of the newly submitted proposal.
pub async fn submit_bless_replica_version_proposal(
    governance: &Canister<'_>,
    sender: Sender,
    neuron_id: NeuronId,
    version: ReplicaVersion,
    sha256: String,
    upgrade_url: String,
) -> ProposalId {
    submit_external_update_proposal_allowing_error(
        governance,
        sender,
        neuron_id,
        NnsFunction::BlessReplicaVersion,
        BlessReplicaVersionPayload {
            replica_version_id: String::from(version.clone()),
            binary_url: "".into(),
            sha256_hex: "".into(),
            node_manager_binary_url: "".into(),
            node_manager_sha256_hex: "".into(),
            release_package_url: upgrade_url,
            release_package_sha256_hex: sha256.clone(),
        },
        format!(
            "Bless replica version: {} with hash: {}",
            String::from(version),
            sha256
        ),
        "".to_string(),
    )
    .await
    .expect("submit_bless_replica_version_proposal failed")
}

/// Submits a proposal for updating a subnet replica software version.
///
/// # Arguments
///
/// * `governance`  - Governance canister
/// * `sender`      - Sender of the proposal
/// * `neuron_id`   - ID of the proposing neuron. This neuron will automatically
///   vote in favor of the proposal.
/// * `version`     - Replica software version
/// * `subnet_id`   - ID of the subnet to be updated
///
/// Note: The existing replica *must* check that the new replica image
/// has the expected SHA256. If there is a mismatch, then this proposal
/// must eventually fail.
///
/// Eventually returns the identifier of the newly submitted proposal.
pub async fn submit_update_subnet_replica_version_proposal(
    governance: &Canister<'_>,
    sender: Sender,
    neuron_id: NeuronId,
    version: ReplicaVersion,
    subnet_id: SubnetId,
) -> ProposalId {
    submit_external_update_proposal_allowing_error(
        governance,
        sender,
        neuron_id,
        NnsFunction::UpdateSubnetReplicaVersion,
        UpdateSubnetReplicaVersionPayload {
            subnet_id: subnet_id.get(),
            replica_version_id: String::from(version.clone()),
        },
        format!(
            "Update {} subnet's replica version to: {}",
            subnet_id,
            String::from(version)
        ),
        "".to_string(),
    )
    .await
    .expect("submit_update_subnet_replica_version_proposal failed")
}

/// Submits a proposal for creating an application subnet.
///
/// # Arguments
///
/// * `governance`      - Governance canister
/// * `node_ids`        - IDs of (currently, unassigned) nodes that should join
///   the new subnet
/// * `replica_version` - Replica software version to install to the new subnet
///   nodes (see `get_software_version`)
///
/// Eventually returns the identifier of the newly submitted proposal.
pub async fn submit_create_application_subnet_proposal(
    governance: &Canister<'_>,
    node_ids: Vec<NodeId>,
    replica_version: ReplicaVersion,
) -> ProposalId {
    let config =
        subnet_configuration::get_default_config_params(SubnetType::Application, node_ids.len());
    let gossip = p2p::build_default_gossip_config();
    let scheduler = SchedulerConfig::application_subnet();
    let payload = CreateSubnetPayload {
        node_ids,
        subnet_id_override: None,
        ingress_bytes_per_block_soft_cap: config.ingress_bytes_per_block_soft_cap,
        max_ingress_bytes_per_message: config.max_ingress_bytes_per_message,
        max_ingress_messages_per_block: config.max_ingress_messages_per_block,
        max_block_payload_size: config.max_block_payload_size,
        replica_version_id: replica_version.to_string(),
        unit_delay_millis: duration_to_millis(config.unit_delay),
        initial_notary_delay_millis: duration_to_millis(config.initial_notary_delay),
        dkg_interval_length: config.dkg_interval_length.get(),
        dkg_dealings_per_block: config.dkg_dealings_per_block as u64,
        gossip_max_artifact_streams_per_peer: gossip.max_artifact_streams_per_peer,
        gossip_max_chunk_wait_ms: gossip.max_chunk_wait_ms,
        gossip_max_duplicity: gossip.max_duplicity,
        gossip_max_chunk_size: gossip.max_chunk_size,
        gossip_receive_check_cache_size: gossip.receive_check_cache_size,
        gossip_pfn_evaluation_period_ms: gossip.pfn_evaluation_period_ms,
        gossip_registry_poll_period_ms: gossip.registry_poll_period_ms,
        gossip_retransmission_request_ms: gossip.retransmission_request_ms,
        advert_best_effort_percentage: gossip.advert_config.map(|gac| gac.best_effort_percentage),
        start_as_nns: false,
        subnet_type: SubnetType::Application,
        is_halted: false,
        max_instructions_per_message: scheduler.max_instructions_per_message.get(),
        max_instructions_per_round: scheduler.max_instructions_per_round.get(),
        max_instructions_per_install_code: scheduler.max_instructions_per_install_code.get(),
        features: SubnetFeatures::default(),
        max_number_of_canisters: 4,
        ssh_readonly_access: vec![],
        ssh_backup_access: vec![],
        ecdsa_config: None,
    };

    submit_external_proposal_with_test_id(governance, NnsFunction::CreateSubnet, payload).await
}

// Queries the registry for the subnet_list record, awaits, decodes, and returns
// the response.
pub async fn get_subnet_list_from_registry(client: &RegistryCanister) -> Vec<SubnetId> {
    let (original_subnets_enc, _) = client
        .get_value(make_subnet_list_record_key().as_bytes().to_vec(), None)
        .await
        .expect("failed to get value for subnet list");

    deserialize_registry_value::<SubnetListRecord>(Ok(Some(original_subnets_enc)))
        .expect("could not decode subnet list record")
        .unwrap()
        .subnets
        .iter()
        .map(|s| SubnetId::from(PrincipalId::try_from(s.clone().as_slice()).unwrap()))
        .collect::<Vec<SubnetId>>()
}

/// Submits a proposal for updating replica software version of unassigned
/// nodes.
///
/// # Arguments
///
/// * `governance`          - Governance canister
/// * `sender`              - Sender of the proposal
/// * `neuron_id`           - ID of the proposing neuron. This neuron will
///   automatically vote in favor of the proposal.
/// * `version`             - Replica software version
/// * `readonly_public_key` - Public key of ssh credentials for readonly access
///   to the node.
///
/// Eventually returns the identifier of the newly submitted proposal.
pub async fn submit_update_unassigned_node_version_proposal(
    governance: &Canister<'_>,
    sender: Sender,
    neuron_id: NeuronId,
    version: String,
    readonly_public_key: String,
) -> ProposalId {
    submit_external_update_proposal_allowing_error(
        governance,
        sender,
        neuron_id,
        NnsFunction::UpdateUnassignedNodesConfig,
        UpdateUnassignedNodesConfigPayload {
            ssh_readonly_access: Some(vec![readonly_public_key]),
            replica_version: Some(version.clone()),
        },
        format!("Update unassigned nodes version to: {}", version.clone()),
        "".to_string(),
    )
    .await
    .expect("submit_update_unassigned_node_version_proposal failed")
}
