use dfn_candid::candid;
use dfn_core::{
    api::caller,
    endpoint::{over, over_async},
    stable,
};
use ic_base_types::PrincipalId;
use ic_nervous_system_root::{
    change_canister, AddCanisterProposal, ChangeCanisterProposal, StopOrStartCanisterProposal,
    LOG_PREFIX,
};
use ic_nns_common::access_control::check_caller_is_governance;
use ic_nns_handler_root::{
    canister_management,
    root_proposals::{GovernanceUpgradeRootProposal, RootProposalBallot},
};

fn main() {}

#[cfg(target_arch = "wasm32")]
use dfn_core::println;
use ic_nns_handler_root::canister_management::do_add_nns_canister;

// canister_init and canister_post_upgrade are needed here
// to ensure that printer hook is set up, otherwise error
// messages are quite obscure.
#[export_name = "canister_init"]
fn canister_init() {
    dfn_core::printer::hook();
    println!("{}canister_init", LOG_PREFIX);
}

#[export_name = "canister_post_upgrade"]
fn canister_post_upgrade() {
    dfn_core::printer::hook();
    println!("{}canister_post_upgrade", LOG_PREFIX);
    // Wipe out stable memory, because earlier version of this canister were
    // stateful. This minimizes risk of future mis-interpretation of data.
    stable::set(&[]);
}

ic_nervous_system_common_build_metadata::define_get_build_metadata_candid_method! {}

/// Returns the status of the canister specified in the input.
///
/// The status of NNS canisters should be public information: anyone can get the
/// status of any NNS canister.
///
/// This must be an update, not a query, because an inter-canister call to the
/// management canister is required.
#[export_name = "canister_update canister_status"]
fn canister_status() {
    println!("{}canister_status", LOG_PREFIX);
    over_async(candid, ic_nervous_system_root::canister_status)
}

#[export_name = "canister_update submit_change_nns_canister_proposal"]
fn submit_change_nns_canister_proposal() {
    panic!(
        "This method was removed in PR 11215. \
            Use instead function `manage_neuron` on the Governance canister \
            to submit a proposal to change an NNS canister."
    );
}

#[export_name = "canister_update submit_root_proposal_to_upgrade_governance_canister"]
fn submit_root_proposal_to_upgrade_governance_canister() {
    over_async(
        candid,
        |(expected_governance_wasm_sha, proposal): (Vec<u8>, ChangeCanisterProposal)| {
            ic_nns_handler_root::root_proposals::submit_root_proposal_to_upgrade_governance_canister(
                caller(),
                expected_governance_wasm_sha,
                proposal,
            )
        },
    );
}

#[export_name = "canister_update vote_on_root_proposal_to_upgrade_governance_canister"]
fn vote_on_root_proposal_to_upgrade_governance_canister() {
    over_async(
        candid,
        |(proposer, wasm_sha256, ballot): (PrincipalId, Vec<u8>, RootProposalBallot)| {
            ic_nns_handler_root::root_proposals::vote_on_root_proposal_to_upgrade_governance_canister(
                caller(),
                proposer,
                wasm_sha256,
                ballot,
            )
        },
    );
}

#[export_name = "canister_update get_pending_root_proposals_to_upgrade_governance_canister"]
fn get_pending_root_proposals_to_upgrade_governance_canister() {
    over(candid, |()| -> Vec<GovernanceUpgradeRootProposal> {
        ic_nns_handler_root::root_proposals::get_pending_root_proposals_to_upgrade_governance_canister()
    })
}

/// Executes a proposal to change an NNS canister.
#[export_name = "canister_update change_nns_canister"]
fn change_nns_canister() {
    check_caller_is_governance();

    // We want to reply first, so that in the case that we want to upgrade the
    // governance canister, the root canister no longer holds a pending callback
    // to it -- and therefore does not prevent the proposals canister from being
    // stopped.
    //
    // To do so, we use `over` instead of the more common `over_async`.
    //
    // This will effectively reply synchronously with the first call to the
    // management canister in change_canister.
    over(candid, |(proposal,): (ChangeCanisterProposal,)| {
        // Because change_canister is async, and because we can't directly use
        // `await`, we need to use the `spawn` trick.
        let future = change_canister(proposal);

        // Starts the proposal execution, which will continue after this function has
        // returned.
        dfn_core::api::futures::spawn(future);
    });
}

#[export_name = "canister_update add_nns_canister"]
fn add_nns_canister() {
    check_caller_is_governance();
    over_async(candid, |(proposal,): (AddCanisterProposal,)| async move {
        do_add_nns_canister(proposal).await;
    });
}

// Executes a proposal to stop/start an nns canister.
#[export_name = "canister_update stop_or_start_nns_canister"]
fn stop_or_start_nns_canister() {
    check_caller_is_governance();
    over_async(
        candid,
        |(proposal,): (StopOrStartCanisterProposal,)| async move {
            // Can't stop/start the governance canister since that would mean
            // we couldn't submit any more proposals.
            // Since this canister is the only possible caller, it's then safe
            // to call stop/start inline.
            if proposal.canister_id == ic_nns_constants::GOVERNANCE_CANISTER_ID
                || proposal.canister_id == ic_nns_constants::ROOT_CANISTER_ID
                || proposal.canister_id == ic_nns_constants::LIFELINE_CANISTER_ID
            {
                panic!("The governance, root and lifeline canisters can't be stopped or started.")
            }
            canister_management::stop_or_start_nns_canister(proposal).await
        },
    );
}
