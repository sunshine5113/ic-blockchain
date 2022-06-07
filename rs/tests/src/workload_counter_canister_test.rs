/* tag::catalog[]

Title:: Use workload to execute update/query calls on counter canisters.

Goal:: Ensure that at a moderate rate of requests per second, workload sends update/query requests to counter canisters successfully.
Update calls increments canisters counter. Query calls (with non-existing methods) on canisters are expected to fail.

Runbook::
0. Set up an IC with an application subnet.
1. Install X counter canisters on this subnet.
2. Instantiate and start the workload.
   Workload sends update/query requests to counter canisters in a round-robin fashion:
   [
       update[canister_id_0, "write"], // should be successful
       query[canister_id_1, "non_existing_method_a"], // should fail
       query[canister_id_0, "non_existing_method_b"], // should fail
       update[canister_id_1, "write"], // should be successful
    ].
   These requests are sent to a random node of an application subnet.
3. Assert the expected number of failed query calls on each canister.
4. Assert the expected number of successful update calls on each canister.

end::catalog[] */

use crate::driver::ic::{InternetComputer, Subnet};
use crate::nns::NnsExt;
use crate::util::{
    assert_canister_counter_with_retries, assert_create_agent, assert_endpoints_reachability,
    block_on, delay, get_random_application_node_endpoint, EndpointsStatus,
};
use crate::workload::{CallSpec, Request, RoundRobinPlan, Workload};
use ic_agent::{export::Principal, Agent};
use ic_fondue::ic_manager::IcHandle;
use ic_prep_lib::subnet_configuration::constants;
use ic_registry_subnet_type::SubnetType;
use ic_utils::interfaces::ManagementCanister;
use slog::info;
use std::time::Duration;

const NODES_COUNT: usize = 3;
const NON_EXISTING_METHOD_A: &str = "non_existing_method_a";
const NON_EXISTING_METHOD_B: &str = "non_existing_method_b";
const MAX_RETRIES: u32 = 10;
const RETRY_WAIT: Duration = Duration::from_secs(10);
const SUCCESS_THRESHOLD: f32 = 0.95; // If more than 95% of the expected calls are successful the test passes
const RESPONSES_COLLECTION_EXTRA_TIMEOUT: Duration = Duration::from_secs(5); // Responses are collected during the workload execution + this extra time, after all requests had been dispatched.

/// Default configuration for this test
pub fn config() -> InternetComputer {
    InternetComputer::new().add_subnet(Subnet::new(SubnetType::Application).add_nodes(NODES_COUNT))
}

/// SLO test configuration with a NNS subnet and an app subnet with the same number of nodes as used on mainnet
pub fn two_third_latency_config() -> InternetComputer {
    InternetComputer::new()
        .add_subnet(Subnet::new(SubnetType::System).add_nodes(40))
        .add_subnet(
            Subnet::new(SubnetType::Application).add_nodes(constants::SMALL_APP_SUBNET_MAX_SIZE),
        )
}

/// Default test installing two canisters and sending 60 requests per second for 30 seconds
/// This test is run in hourly jobs.
pub fn short_test(handle: IcHandle, ctx: &ic_fondue::pot::Context) {
    let canister_count: usize = 2;
    let rps: usize = 60;
    let duration: Duration = Duration::from_secs(30);
    test(handle, ctx, canister_count, rps, duration);
}

/// SLO test installing two canisters and sending 200 requests per second for 500 seconds.
/// This test is run nightly.
pub fn two_third_latency_test(handle: IcHandle, ctx: &ic_fondue::pot::Context) {
    // Install NNS canisters
    ctx.install_nns_canisters(&handle, true);
    let canister_count: usize = 2;
    let rps: usize = 200;
    let duration: Duration = Duration::from_secs(500);
    test(handle, ctx, canister_count, rps, duration);
}

fn test(
    handle: IcHandle,
    ctx: &ic_fondue::pot::Context,
    canister_count: usize,
    rps: usize,
    duration: Duration,
) {
    let mut rng = ctx.rng.clone();
    let app_endpoint = get_random_application_node_endpoint(&handle, &mut rng);
    let endpoints: Vec<_> = handle.as_permutation(&mut rng).collect();
    block_on(async move {
        // Assert all nodes are reachable via http:://[IPv6]:8080/api/v2/status
        assert_endpoints_reachability(endpoints.as_slice(), EndpointsStatus::AllReachable).await;
        info!(ctx.logger, "All nodes are reachable, IC setup succeeded.");

        info!(
            ctx.logger,
            "Step 1: Install {} canisters on the subnet..", canister_count
        );
        let mut agents = Vec::new();
        let mut canisters = Vec::new();

        agents.push(assert_create_agent(app_endpoint.url.as_str()).await);
        let install_agent = agents[0].clone();
        for _ in 0..canister_count {
            canisters.push(install_counter_canister(&install_agent).await);
        }
        info!(
            ctx.logger,
            "{} canisters installed successfully.",
            canisters.len()
        );
        assert_eq!(
            canisters.len(),
            canister_count,
            "Not all canisters deployed successfully, installed {:?} expected {:?}",
            canisters.len(),
            canister_count
        );
        info!(ctx.logger, "Step 2: Instantiate and start the workload..");
        let payload: Vec<u8> = vec![0; 12];
        let plan = RoundRobinPlan::new(vec![
            Request::Update(CallSpec::new(canisters[0], "write", payload.clone())),
            Request::Query(CallSpec::new(
                canisters[1],
                NON_EXISTING_METHOD_A,
                payload.clone(),
            )),
            Request::Query(CallSpec::new(
                canisters[0],
                NON_EXISTING_METHOD_B,
                payload.clone(),
            )),
            Request::Update(CallSpec::new(canisters[1], "write", payload.clone())),
        ]);
        let workload = Workload::new(agents, rps, duration, plan, ctx.logger.clone())
            .with_responses_collection_extra_timeout(RESPONSES_COLLECTION_EXTRA_TIMEOUT);
        let metrics = workload
            .execute()
            .await
            .expect("Workload execution has failed.");
        info!(
            ctx.logger,
            "Step 3: Assert expected number of failed query calls on each canister.."
        );
        let requests_count = rps * duration.as_secs() as usize;
        // 1/2 requests are query (failure) and 1/2 are update (success).
        let expected_failure_calls = requests_count / 2;
        let expected_success_calls = requests_count / 2;
        let errors = metrics.errors();
        assert!(errors.len() < 4, "More errors than expected: {:?}", errors);
        // Error messages should contain the name of failed method call.
        assert!(
            errors.keys().any(|k| k.contains(NON_EXISTING_METHOD_A)),
            "Missing error key {}",
            NON_EXISTING_METHOD_A
        );
        assert!(
            errors.keys().any(|k| k.contains(NON_EXISTING_METHOD_B)),
            "Missing error key {}",
            NON_EXISTING_METHOD_B
        );
        assert!(
            errors
                .values()
                .all(|k| *k == expected_failure_calls / canister_count),
            "Observed number of failure calls is not {}",
            expected_failure_calls / canister_count
        );
        assert_eq!(
            metrics.failure_calls(),
            expected_failure_calls,
            "Observed failure calls {}, expected failure calls {}",
            metrics.failure_calls(),
            expected_failure_calls
        );
        assert!(
            metrics.success_calls()
                > (SUCCESS_THRESHOLD * (expected_success_calls as f32)) as usize,
            "Observed success calls {}, expected success calls {}",
            metrics.success_calls(),
            expected_success_calls
        );
        assert_eq!(
            requests_count,
            metrics.total_calls(),
            "Sent requests {}, recorded number of total calls {}",
            requests_count,
            metrics.total_calls()
        );
        info!(
            ctx.logger,
            "Step 4: Assert the expected number of update calls on each canister.."
        );
        let expected_canister_counter =
            (SUCCESS_THRESHOLD * (expected_success_calls as f32)) as usize / canister_count;
        for canister in canisters.iter() {
            assert_canister_counter_with_retries(
                &ctx.logger,
                &install_agent,
                canister,
                payload.clone(),
                expected_canister_counter,
                MAX_RETRIES,
                RETRY_WAIT,
            )
            .await;
        }
    });
}

pub async fn install_counter_canister(agent: &Agent) -> Principal {
    const COUNTER_CANISTER_WAT: &[u8] = include_bytes!("./counter.wat");
    let mgr = ManagementCanister::create(agent);

    let canister_id = mgr
        .create_canister()
        .as_provisional_create_with_amount(None)
        .call_and_wait(delay())
        .await
        .unwrap()
        .0;

    mgr.install_code(
        &canister_id,
        wabt::wat2wasm(COUNTER_CANISTER_WAT).unwrap().as_slice(),
    )
    .call_and_wait(delay())
    .await
    .expect("Failed to install counter canister.");

    canister_id
}
