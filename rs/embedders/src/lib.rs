pub mod cow_memory_creator;
mod dispatcher;
mod signal_handler;
pub mod wasm_executor;
pub mod wasmtime_embedder;

pub use dispatcher::QueueConfig;
pub use dispatcher::ReturnToken;
pub use dispatcher::RunnerConfig;
pub use dispatcher::RunnerInput;
pub use dispatcher::RunnerOutput;
use ic_cycles_account_manager::CyclesAccountManager;
use ic_interfaces::execution_environment::{HypervisorError, InstanceStats, SubnetAvailableMemory};
use ic_replicated_state::{
    canister_state::system_state::SystemState, ExecutionState, Global, PageIndex,
};
use ic_system_api::ApiType;
use ic_types::{
    ingress::WasmResult, methods::FuncRef, ComputeAllocation, NumBytes, NumInstructions,
};
use std::sync::Arc;
pub use wasmtime_embedder::{WasmtimeEmbedder, WasmtimeMemoryCreator};

// An async result of wasm execution.
// Cannot be cloned. Can only be consumed.
pub struct WasmExecutionResult {
    pub output_receiver: crossbeam_channel::Receiver<RunnerOutput>,
}

impl WasmExecutionResult {
    pub fn get(self) -> WasmExecutionOutput {
        let res = self
            .output_receiver
            .recv()
            .expect("Recv failed: WasmRunner apparently died");

        WasmExecutionResult::on_result(res)
    }

    fn on_result(res: RunnerOutput) -> WasmExecutionOutput {
        res.output
    }
}

pub struct WasmExecutionInput {
    pub api_type: ApiType,
    pub system_state: SystemState,
    pub instructions_limit: NumInstructions,
    pub canister_memory_limit: NumBytes,
    pub canister_current_memory_usage: NumBytes,
    pub subnet_available_memory: SubnetAvailableMemory,
    pub compute_allocation: ComputeAllocation,
    pub func_ref: FuncRef,
    pub execution_state: ExecutionState,
    pub cycles_account_manager: Arc<CyclesAccountManager>,
}

pub struct WasmExecutionOutput {
    pub wasm_result: Result<Option<WasmResult>, HypervisorError>,
    pub num_instructions_left: NumInstructions,
    pub system_state: SystemState,
    pub execution_state: ExecutionState,
    pub instance_stats: InstanceStats,
}

pub struct InstanceRunResult {
    pub dirty_pages: Vec<PageIndex>,
    pub exported_globals: Vec<Global>,
}

pub trait LinearMemory {
    fn as_ptr(&self) -> *mut libc::c_void;

    fn grow_mem_to(&self, _new_size: u32) {}
}

pub trait ICMemoryCreator {
    type Mem: LinearMemory;

    fn new_memory(
        &self,
        mem_size: usize,
        guard_size: usize,
        instance_heap_offset: usize,
        min_pages: u32,
        max_pages: Option<u32>,
    ) -> Self::Mem;
}
