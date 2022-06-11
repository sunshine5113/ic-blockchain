use clap::Parser;
use ic_fondue::pot::execution::TestResult;
use ic_fondue::result::{propagate_children_results_to_parents, TestResultNode};
use ic_tests::driver::cli::{
    CliArgs, DriverSubCommand, ValidatedCliProcessTestsArgs, ValidatedCliRunTestsArgs,
};
use ic_tests::driver::config;
use ic_tests::driver::driver_setup::{create_driver_context_from_cli, initialize_env, mk_logger};
use ic_tests::driver::evaluation::{evaluate, generate_suite_execution_contract};
use ic_tests::driver::ic::VmAllocationStrategy;
use ic_tests::driver::pot_dsl::*;
use ic_tests::driver::test_env::TestEnv;
use ic_tests::{
    api_test, basic_health_test, boundary_nodes_integration, consensus, execution, ledger_tests,
    message_routing, networking, nns_tests, orchestrator, rosetta_test, spec_compliance, tecdsa,
    wasm_generator_test, workload_counter_canister_test,
};
use regex::Regex;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

fn run_tests(validated_args: ValidatedCliRunTestsArgs) -> anyhow::Result<()> {
    let mut suite = match get_test_suites().remove(&validated_args.suite) {
        Some(s) => s,
        None => anyhow::bail!(format!("Test suite {} is undefined", &validated_args.suite)),
    };
    apply_filters(
        &mut suite,
        &validated_args.include_pattern,
        &validated_args.ignore_pattern,
        &validated_args.skip_pattern,
    );
    let system_env = validated_args.working_dir.join(config::SYSTEM_ENV_DIR);
    fs::create_dir(&system_env)?;
    let logger = mk_logger();
    let env = TestEnv::new(system_env, logger.clone())?;
    initialize_env(&env, validated_args.clone())?;
    let context = create_driver_context_from_cli(validated_args, env, get_hostname());
    // Contract file is used in the processing stage of the test results.
    // It is necessary to infer if tests/pots/suite were executed successfully according to the plan.
    let suite_contract = generate_suite_execution_contract(&suite);
    context
        .env
        .write_json_object(config::TEST_SUITE_CONTRACT_FILE, &suite_contract)
        .expect("Couldn't save test suite execution contract file.");
    // Run all tests. Each test dumps an execution result file, which indicates whether this test has succeeded or failed (with an error message).
    evaluate(&context, suite);
    Ok(())
}

fn process_test_results(validated_args: ValidatedCliProcessTestsArgs) -> anyhow::Result<()> {
    let working_dir: PathBuf = validated_args.working_dir;
    // Final result object to be populated/updated with individual test results and saved.
    let mut suite_result: TestResultNode = {
        let suite_contract_file = working_dir
            .join(Path::new(config::SYSTEM_ENV_DIR).join(config::TEST_SUITE_CONTRACT_FILE));
        let file = fs::File::open(&suite_contract_file)
            .unwrap_or_else(|_| panic!("Could not open: {:?}", suite_contract_file));
        let suite_contract = serde_json::from_reader(&file)
            .unwrap_or_else(|_| panic!("Could not read json. {:?}", &file));
        TestResultNode::from(&suite_contract)
    };
    // Walk over all test result files and update suite object with corresponding results.
    let test_depth_level: usize = 1;
    let pot_depth_level: usize = 3;
    for entry in WalkDir::new(working_dir) {
        let path: PathBuf = entry.unwrap().into_path();
        let file_name: &str = path.file_name().unwrap().to_str().unwrap();
        if !file_name.contains(config::TEST_RESULT_FILE) {
            continue;
        }
        let test_name = path
            .ancestors()
            .nth(test_depth_level)
            .unwrap()
            .file_name()
            .unwrap();
        let pot_name = path
            .ancestors()
            .nth(pot_depth_level)
            .unwrap()
            .file_name()
            .unwrap();
        let test_result: TestResultNode = {
            let file =
                fs::File::open(&path).unwrap_or_else(|_| panic!("Could not open: {:?}", path));
            serde_json::from_reader(&file)
                .unwrap_or_else(|_| panic!("Could not read json. {:?}", &file))
        };
        // Update suite result object with a test result.
        let test: &mut TestResultNode = suite_result
            .children
            .iter_mut()
            .find(|p| p.name.as_str() == pot_name)
            .unwrap_or_else(|| {
                panic!(
                    "Couldn't match the pot={} in the suite contract.",
                    pot_name.to_str().unwrap()
                )
            })
            .children
            .iter_mut()
            .find(|t| t.name.as_str() == test_name)
            .unwrap_or_else(|| {
                panic!(
                    "Couldn't match the test={} in the suite contract.",
                    test_name.to_str().unwrap()
                )
            });
        *test = test_result;
    }
    // Recursively infer suite and pot level results from individual tests results.
    propagate_children_results_to_parents(&mut suite_result);
    // Save the final result file of the test suite.
    let suite_result_file = validated_args
        .test_result_dir
        .join(config::TEST_SUITE_RESULT_FILE);
    let content = serde_json::to_string_pretty(&suite_result)
        .unwrap_or_else(|e| panic!("Could not serialize suite result to string. error={:?}", e));
    fs::write(suite_result_file, content)
        .unwrap_or_else(|e| panic!("Could not save test suite result to a file. error={:?}", e));
    if suite_result.result != TestResult::Passed {
        anyhow::bail!(format!("Test suite {} failed", suite_result.name))
    } else {
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    let cli_args = CliArgs::parse();
    match cli_args.action {
        DriverSubCommand::RunTests(args) => {
            let validated_args = args.validate()?;
            run_tests(validated_args)
        }
        DriverSubCommand::ProcessTestResults(args) => {
            let validated_args = args.validate()?;
            process_test_results(validated_args)
        }
    }
}

fn get_hostname() -> Option<String> {
    std::env::var("HOSTNAME").ok()
}

fn apply_filters(
    suite: &mut Suite,
    include: &Option<Regex>,
    ignore: &Option<Regex>,
    skip: &Option<Regex>,
) {
    for p in suite.pots.iter_mut() {
        let tests = match &mut p.testset {
            TestSet::Parallel(tests) => tests,
            TestSet::Sequence(tests) => tests,
        };
        for t in tests.iter_mut() {
            let path = TestPath::new()
                .join(suite.name.clone())
                .join(p.name.clone())
                .join(t.name.clone());
            t.execution_mode = resolve_execution_mode(&format!("{}", path), include, ignore, skip);
        }
        // At least one test is qualified for running. A corresponding pot needs to be
        // set up.
        if tests.iter().any(|t| t.execution_mode == ExecutionMode::Run) {
            continue;
        }
        // At least one test is skipped. The pot needs to be included in a summary.
        if tests
            .iter()
            .any(|t| t.execution_mode == ExecutionMode::Skip)
        {
            p.execution_mode = ExecutionMode::Skip;
            continue;
        }
        p.execution_mode = ExecutionMode::Ignore;
    }
}

fn resolve_execution_mode(
    name: &str,
    include: &Option<Regex>,
    ignore: &Option<Regex>,
    skip: &Option<Regex>,
) -> ExecutionMode {
    if let Some(i) = include {
        if i.is_match(name) {
            return ExecutionMode::Run;
        }
        return ExecutionMode::Ignore;
    }
    if let Some(i) = ignore {
        if i.is_match(name) {
            return ExecutionMode::Ignore;
        }
    }
    if let Some(s) = skip {
        if s.is_match(name) {
            return ExecutionMode::Skip;
        }
    }
    ExecutionMode::Run
}

fn get_test_suites() -> HashMap<String, Suite> {
    let mut m = HashMap::new();

    m.add_suite(suite(
        "pre_master",
        vec![
            pot_with_setup(
                "api_test",
                api_test::setup_two_ics,
                par(vec![
                    sys_t(
                        "ics_have_correct_subnet_count",
                        api_test::ics_have_correct_subnet_count,
                    ),
                    // sys_t("vm_control", api_test::vm_control), disabled due to flakiness
                    sys_t("upload_file_to_farm", api_test::upload_file_to_farm),
                    sys_t(
                        "install_counter_canister",
                        api_test::install_counter_canister,
                    ),
                ]),
            ),
            /*
            Disabled due to flakiness
            pot_with_setup(
                "btc_pot",
                btc_integration::btc::config,
                par(vec![
                    sys_t("btc_test", btc_integration::btc::test),
                ]),
            ),
            pot_with_setup(
                "http_pot",
                http_from_canister::basic_http::config,
                par(vec![
                    sys_t("basic_http", http_from_canister::basic_http::test),
                ]),
            ),*/
            pot_with_setup(
                "boundary_nodes_pot",
                boundary_nodes_integration::boundary_nodes::config,
                par(vec![
                    sys_t(
                        "boundary_nodes_test",
                        boundary_nodes_integration::boundary_nodes::test,
                    ),
                    sys_t(
                        "boundary_nodes_nginx_test",
                        boundary_nodes_integration::boundary_nodes::nginx_test,
                    ),
                ]),
            ),
            pot(
                "firewall_pot",
                networking::firewall::config(),
                par(vec![t(
                    "change_to_firewall_rules_takes_effect",
                    networking::firewall::change_to_firewall_rules_takes_effect,
                )]),
            ),
            pot(
                "create_subnet",
                nns_tests::create_subnet::config(),
                par(vec![t("create_subnet", nns_tests::create_subnet::test)]),
            ),
            execution::upgraded_pots::general_execution_pot(),
            execution::upgraded_pots::cycles_restrictions_pot(),
            execution::upgraded_pots::inter_canister_queries(),
            execution::upgraded_pots::compute_allocation_pot(),
            pot(
                "global_reboot_pot",
                message_routing::global_reboot_test::config(),
                par(vec![t(
                    "global_reboot_test",
                    message_routing::global_reboot_test::test,
                )]),
            ),
            pot(
                "node_removal_from_registry_pot",
                nns_tests::node_removal_from_registry::config(),
                par(vec![t(
                    "node_removal_from_registry_test",
                    nns_tests::node_removal_from_registry::test,
                )]),
            ),
            pot_with_setup(
                "node_assign_pot",
                orchestrator::node_assign_test::config,
                par(vec![sys_t(
                    "node_assign_test",
                    orchestrator::node_assign_test::test,
                )]),
            ),
            pot(
                "node_graceful_leaving_pot",
                consensus::node_graceful_leaving_test::config(),
                par(vec![t(
                    "node_graceful_leaving_test",
                    consensus::node_graceful_leaving_test::test,
                )]),
            ),
            pot(
                "nns_follow_pot",
                nns_tests::nns_follow::config(),
                par(vec![t("follow_test", nns_tests::nns_follow::test)]),
            ),
            pot(
                "nns_voting_pot",
                nns_tests::nns_voting::config(),
                par(vec![t("voting_test", nns_tests::nns_voting::test)]),
            ),
            pot(
                "nns_token_balance_pot",
                ledger_tests::token_balance::config(),
                par(vec![t(
                    "token_balance_test",
                    ledger_tests::token_balance::test,
                )]),
            ),
            pot(
                "cycles_minting_pot",
                nns_tests::cycles_minting::config(),
                par(vec![t(
                    "cycles_minting_test",
                    nns_tests::cycles_minting::test,
                )]),
            ),
            pot(
                "nns_voting_fuzzing_poc_pot",
                nns_tests::nns_voting_fuzzing_poc_test::config(),
                par(vec![t(
                    "nns_voting_fuzzing_poc_test",
                    nns_tests::nns_voting_fuzzing_poc_test::test,
                )]),
            ),
            pot(
                "nns_canister_uninstall_pot",
                nns_tests::nns_uninstall_canister_by_proposal::config(),
                par(vec![t(
                    "nns_uninstall_canister_by_proposal_test",
                    nns_tests::nns_uninstall_canister_by_proposal::test,
                )]),
            ),
            pot(
                "nns_canister_upgrade_pot",
                nns_tests::nns_canister_upgrade::config(),
                par(vec![t(
                    "nns_canister_upgrade_test",
                    nns_tests::nns_canister_upgrade::test,
                )]),
            ),
            pot(
                "certified_registry_pot",
                execution::registry_authentication_test::config(),
                par(vec![t(
                    "registry_authentication_test",
                    execution::registry_authentication_test::test,
                )]),
            ),
            pot(
                "transaction_ledger_correctness_pot",
                ledger_tests::transaction_ledger_correctness::config(),
                par(vec![t(
                    "transaction_ledger_correctness_test",
                    ledger_tests::transaction_ledger_correctness::test,
                )]),
            ),
            pot_with_setup(
                "unassigned_node_upgrade_test_pot",
                orchestrator::unassigned_node_upgrade_test::config,
                par(vec![sys_t(
                    "unassigned_node_upgrade_test",
                    orchestrator::unassigned_node_upgrade_test::test,
                )]),
            ),
            pot_with_setup(
                "unstuck_subnet_test_pot",
                orchestrator::unstuck_subnet_test::config,
                par(vec![sys_t(
                    "unstuck_subnet_test",
                    orchestrator::unstuck_subnet_test::test,
                )]),
            ),
            pot_with_setup(
                "ssh_access_to_nodes_pot",
                orchestrator::ssh_access_to_nodes::config,
                seq(vec![
                    sys_t(
                        "root_cannot_authenticate",
                        orchestrator::ssh_access_to_nodes::root_cannot_authenticate,
                    ),
                    sys_t(
                        "readonly_cannot_authenticate_without_a_key",
                        orchestrator::ssh_access_to_nodes::readonly_cannot_authenticate_without_a_key,
                    ),
                    sys_t(
                        "readonly_cannot_authenticate_with_random_key",
                        orchestrator::ssh_access_to_nodes::readonly_cannot_authenticate_with_random_key,
                    ),
                    sys_t(
                        "keys_in_the_subnet_record_can_be_updated",
                        orchestrator::ssh_access_to_nodes::keys_in_the_subnet_record_can_be_updated,
                    ),
                    sys_t(
                        "keys_for_unassigned_nodes_can_be_updated",
                        orchestrator::ssh_access_to_nodes::keys_for_unassigned_nodes_can_be_updated,
                    ),
                    sys_t(
                        "multiple_keys_can_access_one_account",
                        orchestrator::ssh_access_to_nodes::multiple_keys_can_access_one_account,
                    ),
                    sys_t(
                        "multiple_keys_can_access_one_account_on_unassigned_nodes",
                        orchestrator::ssh_access_to_nodes::multiple_keys_can_access_one_account_on_unassigned_nodes,
                    ),
                    sys_t(
                        "updating_readonly_does_not_remove_backup_keys",
                        orchestrator::ssh_access_to_nodes::updating_readonly_does_not_remove_backup_keys,
                    ),
                    sys_t(
                        "can_add_max_number_of_readonly_and_backup_keys",
                        orchestrator::ssh_access_to_nodes::can_add_max_number_of_readonly_and_backup_keys,
                    ),
                    sys_t(
                        "cannot_add_more_than_max_number_of_readonly_or_backup_keys",
                        orchestrator::ssh_access_to_nodes::cannot_add_more_than_max_number_of_readonly_or_backup_keys,
                    ),
                ]),
            ),
        ],
    ));

    let xnet_slo_3_subnets = message_routing::xnet_slo_test::config_hotfix_slo_3_subnets();
    m.add_suite(suite(
        "hotfix",
        vec![pot(
            "xnet_slo_3_subnets_pot",
            xnet_slo_3_subnets.build(),
            par(vec![t(
                "xnet_slo_3_subnets_test",
                xnet_slo_3_subnets.test(),
            )]),
        )],
    ));

    let xnet_slo_3_subnets = message_routing::xnet_slo_test::config_prod_slo_3_subnets();
    let xnet_slo_150_subnets = message_routing::xnet_slo_test::config_prod_slo_150_subnets();
    m.add_suite(suite(
        "prod_slo",
        vec![
            pot(
                "xnet_slo_3_subnets_pot",
                xnet_slo_3_subnets.build(),
                par(vec![t(
                    "xnet_slo_3_subnets_test",
                    xnet_slo_3_subnets.test(),
                )]),
            ),
            pot(
                "xnet_slo_150_subnets_pot",
                xnet_slo_150_subnets.build(),
                par(vec![t(
                    "xnet_slo_150_subnets_test",
                    xnet_slo_150_subnets.test(),
                )]),
            ),
        ],
    ));

    let network_reliability = networking::network_reliability::config_sys_4_nodes_app_4_nodes();
    let xnet_nightly_3_subnets = message_routing::xnet_slo_test::config_nightly_3_subnets();
    let xnet_nightly_29_subnets = message_routing::xnet_slo_test::config_nightly_29_subnets();
    m.add_suite(suite(
        "nightly",
        vec![
            pot(
                "xnet_slo_3_subnets_pot",
                xnet_nightly_3_subnets.build(),
                par(vec![t(
                    "xnet_slo_3_subnets_test",
                    xnet_nightly_3_subnets.test(),
                )]),
            ),
            pot(
                "xnet_slo_29_subnets_pot",
                xnet_nightly_29_subnets.build(),
                par(vec![t(
                    "xnet_slo_29_subnets_test",
                    xnet_nightly_29_subnets.test(),
                )]),
            ),
            pot(
                "two_third_latency_pot",
                workload_counter_canister_test::two_third_latency_config(),
                par(vec![t(
                    "workload_counter_canister_test",
                    workload_counter_canister_test::two_third_latency_test,
                )]),
            )
            .with_vm_allocation(VmAllocationStrategy::DistributeAcrossDcs),
            pot_with_setup(
                "network_reliability_pot",
                network_reliability.build(),
                par(vec![sys_t(
                    "network_reliability_test",
                    network_reliability.test(),
                )]),
            ),
        ],
    ));

    m.add_suite(suite(
        "hourly",
        vec![
            pot_with_setup(
                "basic_health_pot_single_host",
                basic_health_test::config_single_host,
                par(vec![sys_t("basic_health_test", basic_health_test::test)]),
            ),
            pot_with_setup(
                "node_reassignment_pot",
                orchestrator::node_reassignment_test::config,
                par(vec![sys_t(
                    "node_reassignment_test",
                    orchestrator::node_reassignment_test::test,
                )]),
            ),
            pot(
                "token_fault_tolerance_pot",
                ledger_tests::token_fault_tolerance::config(),
                par(vec![t(
                    "token_fault_tolerance_test",
                    ledger_tests::token_fault_tolerance::test,
                )]),
            ),
            pot(
                "create_subnet",
                nns_tests::create_subnet::config(),
                par(vec![t("create_subnet", nns_tests::create_subnet::test)]),
            ),
            pot(
                "rejoin",
                message_routing::rejoin_test::config(),
                par(vec![t("rejoin", message_routing::rejoin_test::test)]),
            ),
            pot(
                "tecdsa_add_nodes_pot",
                tecdsa::tecdsa_add_nodes_test::config(),
                par(vec![t(
                    "test_tecdsa_add_nodes",
                    tecdsa::tecdsa_add_nodes_test::test,
                )]),
            ),
            pot(
                "tecdsa_remove_nodes_pot",
                tecdsa::tecdsa_remove_nodes_test::config(),
                par(vec![t(
                    "test_tecdsa_remove_nodes",
                    tecdsa::tecdsa_remove_nodes_test::test,
                )]),
            ),
            pot_with_setup(
                "tecdsa_signature_same_subnet_pot",
                tecdsa::tecdsa_signature_test::config,
                seq(vec![t(
                    "test_threshold_ecdsa_signature_same_subnet",
                    tecdsa::tecdsa_signature_test::test_threshold_ecdsa_signature_same_subnet,
                )])
            ),
            pot_with_setup(
                "tecdsa_signature_life_cycle",
                tecdsa::tecdsa_signature_test::config_without_ecdsa_on_nns,
                seq(vec![t(
                    "test_threshold_ecdsa_life_cycle",
                    tecdsa::tecdsa_signature_test::test_threshold_ecdsa_life_cycle,
                )])
            ),
            pot_with_setup(
                "tecdsa_signature_from_other_subnet_pot",
                tecdsa::tecdsa_signature_test::config,
                seq(vec![t(
                    "test_threshold_ecdsa_signature_from_other_subnet",
                    tecdsa::tecdsa_signature_test::test_threshold_ecdsa_signature_from_other_subnet,
                )])
            ),
            pot_with_setup(
                "tecdsa_signature_fails_without_cycles_pot",
                tecdsa::tecdsa_signature_test::config,
                seq(vec![t(
                    "test_threshold_ecdsa_signature_fails_without_cycles",
                    tecdsa::tecdsa_signature_test::test_threshold_ecdsa_signature_fails_without_cycles,
                )])
            ),
            pot_with_setup(
                "tecdsa_signature_from_nns_without_cycles_pot",
                tecdsa::tecdsa_signature_test::config,
                seq(vec![t(
                    "test_threshold_ecdsa_signature_from_nns_without_cycles",
                    tecdsa::tecdsa_signature_test::test_threshold_ecdsa_signature_from_nns_without_cycles,
                )])
            ),
            pot(
                "workload_counter_canister_pot",
                workload_counter_canister_test::config(),
                par(vec![t(
                    "workload_counter_canister_test",
                    workload_counter_canister_test::short_test,
                )]),
            ),
            pot_with_setup(
                "nns_backup_pot",
                orchestrator::nns_backup::config,
                par(vec![sys_t("nns_backup_test", orchestrator::nns_backup::test,
                )]),
            )
        ],
    ));

    // The tests in this suite require canisters to be build prior to
    // running the tests which is why we separate it out.
    m.add_suite(suite(
        "wasm_generator",
        vec![pot(
            "wasm_generator_pot",
            wasm_generator_test::config(),
            par(vec![t("wasm_generator_test", wasm_generator_test::test)]),
        )],
    ));

    m.add_suite(suite(
        "subnet_recovery",
        vec![
            pot_with_setup(
                "subnet_recovery_app_same_nodes",
                orchestrator::subnet_recovery_app_subnet::setup_same_nodes,
                par(vec![sys_t(
                    "subnet_recovery_app_same_nodes",
                    orchestrator::subnet_recovery_app_subnet::test,
                )]),
            ),
            pot_with_setup(
                "subnet_recovery_app_failover_nodes",
                orchestrator::subnet_recovery_app_subnet::setup_failover_nodes,
                par(vec![sys_t(
                    "subnet_recovery_app_failover_nodes",
                    orchestrator::subnet_recovery_app_subnet::test,
                )]),
            ),
        ],
    ));

    m.add_suite(suite(
        "upgrade_compatibility",
        vec![
            pot_with_setup(
                "upgrade_downgrade_app_subnet",
                orchestrator::upgrade_downgrade::config,
                par(vec![sys_t(
                    "upgrade_downgrade_app_subnet",
                    orchestrator::upgrade_downgrade::upgrade_downgrade_app_subnet,
                )]),
            ),
            pot_with_setup(
                "upgrade_downgrade_nns_subnet",
                orchestrator::upgrade_downgrade::config,
                par(vec![sys_t(
                    "upgrade_downgrade_nns_subnet",
                    orchestrator::upgrade_downgrade::upgrade_downgrade_nns_subnet,
                )]),
            ),
        ],
    ));

    m.add_suite(suite(
        "rosetta",
        vec![pot(
            "rosetta_pot",
            rosetta_test::config(),
            par(vec![t(
                "rosetta_test_everything",
                rosetta_test::test_everything,
            )]),
        )],
    ));

    m.add_suite(suite(
        "spec_compliance",
        vec![
            pot(
                "spec_compliance_with_system_subnet",
                spec_compliance::ic_with_system_subnet(),
                seq(vec![t(
                    "with_system_subnet",
                    spec_compliance::test_system_subnet,
                )]),
            ),
            pot(
                "spec_compliance_with_app_subnet",
                spec_compliance::ic_with_app_subnet(),
                seq(vec![t("with_app_subnet", spec_compliance::test_app_subnet)]),
            ),
        ],
    ));

    m
}

trait TestCatalog {
    fn add_suite(&mut self, suite: Suite);
}

impl TestCatalog for HashMap<String, Suite> {
    fn add_suite(&mut self, suite: Suite) {
        use std::collections::hash_map::Entry;
        if let Entry::Vacant(e) = self.entry(suite.name.clone()) {
            e.insert(suite);
        } else {
            panic!("Redefinition of suite {:?}", suite.name)
        }
    }
}
