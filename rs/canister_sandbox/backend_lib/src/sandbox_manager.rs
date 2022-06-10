//! The sandbox manager provides the actual functionality of the sandbox
//! process. It allows the replica controller process to manage
//! everything required in order to execute code. It holds three
//! kinds of resources that it manages on behalf of the replica
//! controller process:
//!
//! - CanisterWasm: The (wasm) code corresponding to one canister
//! - State: The heap and other (mutable) user state associated with a canister
//! - Execution: An ongoing execution of a canister, using one wasm and state
//!   object
//!
//! All of the above objects as well as the functionality provided
//! towards the controller are found in this module.
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::sync::{Arc, Mutex};

use ic_canister_sandbox_common::protocol::id::{ExecId, MemoryId, WasmId};
use ic_canister_sandbox_common::protocol::sbxsvc::{
    CreateExecutionStateSuccessReply, OpenMemoryRequest,
};
use ic_canister_sandbox_common::protocol::structs::{
    MemoryModifications, SandboxExecInput, SandboxExecOutput, StateModifications,
};
use ic_canister_sandbox_common::{controller_service::ControllerService, protocol};
use ic_config::embedders::Config as EmbeddersConfig;
use ic_embedders::wasm_utils::instrumentation::InstrumentationOutput;
use ic_embedders::wasm_utils::validation::WasmValidationDetails;
use ic_embedders::{
    wasm_executor::WasmStateChanges,
    wasm_utils::{
        decoding::decode_wasm,
        instrumentation::{instrument, InstructionCostTable},
        validation::validate_wasm_binary,
    },
    WasmtimeEmbedder,
};
use ic_interfaces::execution_environment::{ExecutionMode, HypervisorResult, WasmExecutionOutput};
use ic_logger::ReplicaLogger;
use ic_replicated_state::page_map::PageMapSerialization;
use ic_replicated_state::{EmbedderCache, Memory, PageMap};
use ic_types::CanisterId;

use crate::dts::{DeterministicTimeSlicingHandler, PausedExecution};

struct ExecutionInstantiateError;

impl Debug for ExecutionInstantiateError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Failed to instantatiate execution.")
    }
}

/// A canister execution currently in progress.
struct Execution {
    /// Id of the execution. This is used in communicating back to
    /// the replica (e.g. for syscalls) such that replica can associate
    /// events with the correct execution.
    exec_id: ExecId,

    /// The canister wasm used in this execution.
    canister_wasm: Arc<CanisterWasm>,

    /// The sandbox manager that is responsible for
    /// 1) Providing the controller to talk to the replica process.
    /// 2) Creating a new execution state.
    sandbox_manager: Arc<SandboxManager>,
}

impl Execution {
    /// Creates new execution based on canister wasm and state. In order
    /// to start the execution, the given state object will be "locked" --
    /// if that cannot be done, then creation of execution will fail.
    /// The actual code to be run will be scheduled to the given
    /// thread pool.
    ///
    /// This will *actually* schedule and initiate a new execution.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_on_worker_thread(
        exec_id: ExecId,
        canister_wasm: Arc<CanisterWasm>,
        wasm_memory: Arc<Memory>,
        stable_memory: Arc<Memory>,
        sandbox_manager: Arc<SandboxManager>,
        workers: &mut threadpool::ThreadPool,
        exec_input: SandboxExecInput,
        total_timer: std::time::Instant,
    ) {
        let wasm_memory = (*wasm_memory).clone();
        let stable_memory = (*stable_memory).clone();

        let execution = Arc::new(Self {
            exec_id,
            canister_wasm,
            sandbox_manager,
        });

        workers.execute(move || {
            execution.run(exec_id, exec_input, wasm_memory, stable_memory, total_timer)
        });
    }

    // Actual wasm code execution -- this is run on the target thread
    // in the thread pool.
    fn run(
        &self,
        exec_id: ExecId,
        exec_input: SandboxExecInput,
        mut wasm_memory: Memory,
        mut stable_memory: Memory,
        total_timer: std::time::Instant,
    ) {
        let run_timer = std::time::Instant::now();

        let subnet_available_memory = exec_input
            .execution_parameters
            .subnet_available_memory
            .clone();

        let total_instruction_limit = exec_input.execution_parameters.total_instruction_limit;
        let slice_instruction_limit = exec_input.execution_parameters.slice_instruction_limit;
        let sandbox_manager = Arc::clone(&self.sandbox_manager);
        let out_of_instructions_handler = DeterministicTimeSlicingHandler::new(
            total_instruction_limit,
            slice_instruction_limit,
            move |paused_execution| {
                {
                    let mut guard = sandbox_manager.repr.lock().unwrap();
                    guard.paused_executions.insert(exec_id, paused_execution);
                }
                sandbox_manager
                    .controller
                    .execution_paused(protocol::ctlsvc::ExecutionPausedRequest { exec_id });
            },
        );

        let (
            WasmExecutionOutput {
                wasm_result,
                num_instructions_left,
                instance_stats,
            },
            deltas,
            instance_or_system_api,
        ) = ic_embedders::wasm_executor::process(
            exec_input.func_ref,
            exec_input.api_type,
            exec_input.canister_current_memory_usage,
            exec_input.execution_parameters,
            exec_input.sandox_safe_system_state,
            &self.canister_wasm.compilate,
            &self.sandbox_manager.embedder,
            &mut wasm_memory,
            &mut stable_memory,
            &exec_input.globals,
            self.sandbox_manager.log.clone(),
            exec_input.wasm_reserved_pages,
            Arc::new(out_of_instructions_handler),
        );

        match wasm_result {
            Ok(_) => {
                let state_modifications = deltas.map(
                    |WasmStateChanges {
                         dirty_page_indices,
                         globals,
                     }| {
                        let system_state_changes = match instance_or_system_api {
                            // Here we use `store_data_mut` instead of
                            // `into_store_data` because the later will drop the
                            // wasmtime Instance which can be an expensive
                            // operation. Mutating the store instead allows us
                            // to delay the drop until after the execution
                            // completed message is sent back to the main
                            // process.
                            Ok(mut instance) => instance
                                .store_data_mut()
                                .system_api
                                .take_system_state_changes(),
                            Err(system_api) => system_api.into_system_state_changes(),
                        };
                        StateModifications::new(
                            globals,
                            &wasm_memory,
                            &stable_memory,
                            &dirty_page_indices.wasm_memory_delta,
                            &dirty_page_indices.stable_memory_delta,
                            subnet_available_memory.get(),
                            system_state_changes,
                        )
                    },
                );
                if state_modifications.is_some() {
                    self.sandbox_manager
                        .add_memory(exec_input.next_wasm_memory_id, wasm_memory);
                    self.sandbox_manager
                        .add_memory(exec_input.next_stable_memory_id, stable_memory);
                }
                let wasm_output = WasmExecutionOutput {
                    wasm_result,
                    num_instructions_left,
                    instance_stats,
                };
                self.sandbox_manager.controller.execution_finished(
                    protocol::ctlsvc::ExecutionFinishedRequest {
                        exec_id: self.exec_id,
                        exec_output: SandboxExecOutput {
                            wasm: wasm_output,
                            state: state_modifications,
                            execute_total_duration: total_timer.elapsed(),
                            execute_run_duration: run_timer.elapsed(),
                        },
                    },
                );
            }
            Err(err) => {
                let wasm_output = WasmExecutionOutput {
                    wasm_result: Err(err),
                    num_instructions_left,
                    instance_stats,
                };

                self.sandbox_manager.controller.execution_finished(
                    protocol::ctlsvc::ExecutionFinishedRequest {
                        exec_id: self.exec_id,
                        exec_output: SandboxExecOutput {
                            wasm: wasm_output,
                            state: None,
                            execute_total_duration: total_timer.elapsed(),
                            execute_run_duration: run_timer.elapsed(),
                        },
                    },
                );
            }
        }
    }
}

/// Represents a wasm object of a canister. This is the executable code
/// of the canister.
struct CanisterWasm {
    compilate: Arc<EmbedderCache>,
}

impl CanisterWasm {
    /// Validates and compiles the given Wasm binary.
    pub fn compile(
        config: &ic_config::embedders::Config,
        embedder: &Arc<WasmtimeEmbedder>,
        wasm_src: Vec<u8>,
    ) -> HypervisorResult<(Self, InstrumentationOutput, WasmValidationDetails)> {
        let wasm = decode_wasm(Arc::new(wasm_src))?;
        let wasm_validation_details = validate_wasm_binary(&wasm, config)?;
        let instrumentation_output = instrument(
            &wasm,
            &InstructionCostTable::new(),
            config.cost_to_compile_wasm_instruction,
        )?;
        let compilate = embedder.compile(&instrumentation_output.binary)?;
        let compilate = Arc::new(compilate);

        Ok((
            Self { compilate },
            instrumentation_output,
            wasm_validation_details,
        ))
    }
}

/// Manages the entirety of the sandbox process. It provides the methods
/// through which the controller process (the replica) manages the
/// sandboxed execution.
pub struct SandboxManager {
    repr: Mutex<SandboxManagerInt>,
    controller: Arc<dyn ControllerService>,
    embedder: Arc<WasmtimeEmbedder>,
    config: ic_config::embedders::Config,
    log: ReplicaLogger,
}
struct SandboxManagerInt {
    canister_wasms: HashMap<WasmId, Arc<CanisterWasm>>,
    memories: HashMap<MemoryId, Arc<Memory>>,
    paused_executions: HashMap<ExecId, PausedExecution>,
    workers_for_replicated_execution: threadpool::ThreadPool,
    workers_for_non_replicated_execution: threadpool::ThreadPool,
    workers_for_cleanup: threadpool::ThreadPool,
}

impl SandboxManager {
    /// Creates new sandbox manager. In order to operate, it needs
    /// an established backward RPC channel to the controller process
    /// to relay e.g. syscalls and completions.
    pub fn new(
        controller: Arc<dyn ControllerService>,
        config: EmbeddersConfig,
        log: ReplicaLogger,
    ) -> Self {
        let embedder = Arc::new(WasmtimeEmbedder::new(config.clone(), log.clone()));
        SandboxManager {
            repr: Mutex::new(SandboxManagerInt {
                canister_wasms: HashMap::new(),
                memories: HashMap::new(),
                paused_executions: HashMap::new(),
                workers_for_replicated_execution: threadpool::ThreadPool::new(1),
                workers_for_non_replicated_execution: threadpool::ThreadPool::new(
                    config.query_execution_threads,
                ),
                workers_for_cleanup: threadpool::ThreadPool::new(1),
            }),
            controller,
            embedder,
            config,
            log,
        }
    }

    // For internal use in creating execution states we use the outputs of
    // compilation, but these aren't returned by the public RPC.
    fn open_wasm_internal(
        &self,
        wasm_id: WasmId,
        wasm_src: Vec<u8>,
    ) -> HypervisorResult<(
        Arc<CanisterWasm>,
        InstrumentationOutput,
        WasmValidationDetails,
    )> {
        let mut guard = self.repr.lock().unwrap();
        assert!(
            !guard.canister_wasms.contains_key(&wasm_id),
            "Failed to open wasm session {}: id is already in use",
            wasm_id,
        );
        let (wasm, instrumentation_output, wasm_validation_details) =
            CanisterWasm::compile(&self.config, &self.embedder, wasm_src)?;
        // Return as much memory as possible because compiling seems to use up
        // some extra memory that can be returned.
        //
        // SAFETY: 0 is always a valid argument to `malloc_trim`.
        #[cfg(target_os = "linux")]
        unsafe {
            libc::malloc_trim(0);
        }
        let canister_wasm = Arc::new(wasm);
        guard
            .canister_wasms
            .insert(wasm_id, Arc::clone(&canister_wasm));
        Ok((
            canister_wasm,
            instrumentation_output,
            wasm_validation_details,
        ))
    }

    /// Compiles the given Wasm binary and registers it under the given id.
    /// The function may fail if the Wasm binary is invalid.
    pub fn open_wasm(&self, wasm_id: WasmId, wasm_src: Vec<u8>) -> HypervisorResult<()> {
        let (_wasm, _instrumentation_output, _validation_details) =
            self.open_wasm_internal(wasm_id, wasm_src)?;
        Ok(())
    }

    /// Closes previously opened wasm instance, by id.
    pub fn close_wasm(&self, wasm_id: WasmId) {
        let mut guard = self.repr.lock().unwrap();
        let removed = guard.canister_wasms.remove(&wasm_id);
        assert!(
            removed.is_some(),
            "Failed to close wasm session {}: id not found",
            wasm_id
        );
    }

    /// Opens a new memory requested by the replica process.
    pub fn open_memory(&self, request: OpenMemoryRequest) {
        let mut guard = self.repr.lock().unwrap();
        guard.open_memory(request);
    }

    /// Adds a new memory after sandboxed execution.
    fn add_memory(&self, memory_id: MemoryId, memory: Memory) {
        let mut guard = self.repr.lock().unwrap();
        guard.add_memory(memory_id, memory);
    }

    /// Closes previously opened memory instance, by id.
    pub fn close_memory(&self, memory_id: MemoryId) {
        let mut guard = self.repr.lock().unwrap();
        let removed = guard.memories.remove(&memory_id);
        assert!(
            removed.is_some(),
            "Failed to close state {}: id not found",
            memory_id
        );
        // Dropping memory may be expensive. Do it on a worker thread to avoid
        // blocking the main thread of the sandbox process.
        guard.workers_for_cleanup.execute(move || drop(removed));
    }

    /// Starts Wasm execution using specific code and state, passing
    /// execution input.
    ///
    /// Note that inside here we start a transaction and the state of
    /// execution can not and does not change while we are processing
    /// this particular session.
    pub fn start_execution(
        sandbox_manager: &Arc<SandboxManager>,
        exec_id: ExecId,
        wasm_id: WasmId,
        wasm_memory_id: MemoryId,
        stable_memory_id: MemoryId,
        exec_input: SandboxExecInput,
    ) {
        let total_timer = std::time::Instant::now();
        let mut guard = sandbox_manager.repr.lock().unwrap();
        let wasm_runner = guard.canister_wasms.get(&wasm_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: wasm {} not found",
                exec_id, wasm_id
            )
        });
        let wasm_memory = guard.memories.get(&wasm_memory_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: wasm memory {} not found",
                exec_id, wasm_memory_id,
            )
        });
        let stable_memory = guard.memories.get(&stable_memory_id).unwrap_or_else(|| {
            unreachable!(
                "Failed to open exec session {}: stable memory {} not found",
                exec_id, stable_memory_id,
            )
        });
        match exec_input.execution_parameters.execution_mode {
            ExecutionMode::Replicated => Execution::start_on_worker_thread(
                exec_id,
                Arc::clone(wasm_runner),
                Arc::clone(wasm_memory),
                Arc::clone(stable_memory),
                Arc::clone(sandbox_manager),
                &mut guard.workers_for_replicated_execution,
                exec_input,
                total_timer,
            ),
            ExecutionMode::NonReplicated => Execution::start_on_worker_thread(
                exec_id,
                Arc::clone(wasm_runner),
                Arc::clone(wasm_memory),
                Arc::clone(stable_memory),
                Arc::clone(sandbox_manager),
                &mut guard.workers_for_non_replicated_execution,
                exec_input,
                total_timer,
            ),
        };
    }

    /// Resume the paused Wasm execution.
    pub fn resume_execution(sandbox_manager: &Arc<SandboxManager>, exec_id: ExecId) {
        let paused_execution = {
            let mut guard = sandbox_manager.repr.lock().unwrap();
            guard
                .paused_executions
                .remove(&exec_id)
                .unwrap_or_else(|| unreachable!("Failed to get paused execution {}", exec_id))
        };
        paused_execution.resume();
    }

    /// Abort the paused Wasm execution.
    pub fn abort_execution(sandbox_manager: &Arc<SandboxManager>, exec_id: ExecId) {
        let paused_execution = {
            let mut guard = sandbox_manager.repr.lock().unwrap();
            guard
                .paused_executions
                .remove(&exec_id)
                .unwrap_or_else(|| unreachable!("Failed to get paused execution {}", exec_id))
        };
        paused_execution.abort();
    }

    pub fn create_execution_state(
        &self,
        wasm_id: WasmId,
        wasm_source: Vec<u8>,
        wasm_page_map: PageMapSerialization,
        next_wasm_memory_id: MemoryId,
        canister_id: CanisterId,
    ) -> HypervisorResult<CreateExecutionStateSuccessReply> {
        // Validate, instrument, and compile the binary.
        let (canister_wasm, instrumentation_output, wasm_validation_details) =
            self.open_wasm_internal(wasm_id, wasm_source)?;
        let compilation_cost = instrumentation_output.compilation_cost;
        let embedder_cache = Arc::clone(&canister_wasm.compilate);
        let embedder = Arc::clone(&self.embedder);

        let mut wasm_page_map = PageMap::deserialize(wasm_page_map).unwrap();

        let (exported_functions, exported_globals, wasm_memory_delta, wasm_memory_size) =
            ic_embedders::wasm_executor::get_initial_globals_and_memory(
                instrumentation_output,
                &embedder_cache,
                &embedder,
                &mut wasm_page_map,
                canister_id,
            )?;

        let wasm_memory = Memory::new(wasm_page_map, wasm_memory_size);

        // Send all necessary data for creating the execution state to replica.
        let wasm_memory_modifications = MemoryModifications {
            page_delta: wasm_memory.page_map.serialize_delta(&wasm_memory_delta),
            size: wasm_memory_size,
        };

        // Save the memory for future message executions.
        self.add_memory(next_wasm_memory_id, wasm_memory);

        Ok(CreateExecutionStateSuccessReply {
            wasm_memory_modifications,
            exported_globals,
            exported_functions,
            wasm_metadata: wasm_validation_details.wasm_metadata,
            compilation_cost,
        })
    }
}

impl SandboxManagerInt {
    fn open_memory(&mut self, request: OpenMemoryRequest) {
        let page_map = PageMap::deserialize(request.memory.page_map).unwrap();
        let memory = Memory::new(page_map, request.memory.num_wasm_pages);
        self.add_memory(request.memory_id, memory);
    }

    fn add_memory(&mut self, memory_id: MemoryId, memory: Memory) {
        assert!(
            !self.memories.contains_key(&memory_id),
            "Failed to open memory {}: id is already in use",
            memory_id
        );
        let memory = Arc::new(memory);
        self.memories.insert(memory_id, memory);
    }
}
