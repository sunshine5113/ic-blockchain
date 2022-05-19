/* tag::catalog[]
Title:: Firewall

Goal:: Ensure that a change to the firewall settings takes effect

Runbook::
. set up test instance with 1 (NNS) root subnet and one application subnet
. assert that the port 9090 (replica metrics) is reachable
. propose a change to the fw-rules that closes port 9090

Success::
. if a GET request to port 9090 eventually fails
. if a GET request to the public api endpoint still succeeds

end::catalog[] */

use crate::driver::ic::{InternetComputer, Subnet};
use crate::networking::firewall::FEATURE_ACTIVATED;
use crate::nns::{
    submit_external_proposal_with_test_id, vote_execute_proposal_assert_executed, NnsExt,
};
use crate::util::{
    self, block_on, get_random_application_node_endpoint, get_random_nns_node_endpoint,
};
use ic_fondue::ic_manager::IcHandle;
use ic_nns_governance::pb::v1::NnsFunction;
use ic_registry_subnet_type::SubnetType;
use registry_canister::mutations::do_set_firewall_config::SetFirewallConfigPayload;
use reqwest::blocking::Client;
use slog::info;
use std::time::{Duration, Instant};
use url::Url;

const WAIT_TIMEOUT: Duration = Duration::from_secs(60);
const BACKOFF_DELAY: Duration = Duration::from_secs(5);

pub fn config() -> InternetComputer {
    InternetComputer::new()
        .add_subnet(Subnet::fast(SubnetType::System, 1))
        .add_subnet(Subnet::fast(SubnetType::Application, 1))
}

pub fn change_to_firewall_rules_takes_effect(handle: IcHandle, ctx: &ic_fondue::pot::Context) {
    if FEATURE_ACTIVATED {
        // If IC-1026 is activated, this test should not run
        return;
    }

    let log = ctx.logger.clone();
    let mut rng = ctx.rng.clone();
    let http_client = reqwest::blocking::ClientBuilder::new()
        .timeout(BACKOFF_DELAY)
        .build()
        .expect("Could not build reqwest client.");

    ctx.install_nns_canisters(&handle, true);
    let nns_ept = get_random_nns_node_endpoint(&handle, &mut rng);
    let app_ept = get_random_application_node_endpoint(&handle, &mut rng);

    // await for app node to be ready
    block_on(app_ept.assert_ready(ctx));

    // assert that 9090 is readable
    let mut metrics_url = app_ept.url.clone();
    metrics_url.set_port(Some(9090)).unwrap();
    assert!(get_request_succeeds(&log, &http_client, &metrics_url));
    assert!(get_request_succeeds(&log, &http_client, &app_ept.url));

    // prepare proposal to close 9090
    let proposal_payload = prepare_proposal_payload();
    let nns = util::runtime_from_url(nns_ept.url.clone());
    let governance = crate::nns::get_governance_canister(&nns);
    let proposal_id = block_on(submit_external_proposal_with_test_id(
        &governance,
        NnsFunction::SetFirewallConfig,
        proposal_payload,
    ));
    block_on(vote_execute_proposal_assert_executed(
        &governance,
        proposal_id,
    ));

    // wait until 9090 is closed
    let start = Instant::now();
    while get_request_succeeds(&log, &http_client, &metrics_url) {
        // send request
        if start.elapsed() > WAIT_TIMEOUT {
            panic!("Waiting for port 9090 to be closed timed out!");
        }
        std::thread::sleep(BACKOFF_DELAY);
    }
    // assert that 9090 remains closed upon retry
    // i.e., make sure we are not fooled by temporary network failures
    assert!(!get_request_succeeds(&log, &http_client, &metrics_url));
    assert!(get_request_succeeds(&log, &http_client, &app_ept.url));
}

fn get_request_succeeds(log: &slog::Logger, c: &Client, url: &Url) -> bool {
    match c.get(url.clone()).send() {
        Ok(_) => true,
        Err(e) => {
            info!(log, "Get ({}) failed: {:?}", url, e);
            false
        }
    }
}

fn prepare_proposal_payload() -> SetFirewallConfigPayload {
    let cfg = util::get_config();
    let firewall_config = cfg.firewall.unwrap();
    let ipv6_prefixes = firewall_config.ipv6_prefixes;
    let firewall_config = firewall_config.firewall_config.replace("9090, ", "");
    SetFirewallConfigPayload {
        firewall_config,
        ipv4_prefixes: vec![],
        ipv6_prefixes,
    }
}

#[cfg(test)]
mod tests {
    // Checks whether we can mangle/parse the config template.
    #[test]
    fn proposal_can_be_prepared() {
        let _ = super::prepare_proposal_payload();
    }
}
