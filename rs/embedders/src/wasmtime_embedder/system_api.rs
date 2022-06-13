use crate::wasmtime_embedder::{system_api_complexity, StoreData};

use ic_config::flag_status::FlagStatus;
use ic_interfaces::execution_environment::{
    ExecutionComplexity, HypervisorError, HypervisorResult, PerformanceCounterType, SystemApi,
};
use ic_logger::{error, info, ReplicaLogger};
use ic_registry_subnet_type::SubnetType;
use ic_types::{CanisterId, Cycles, NumBytes, NumInstructions};

use wasmtime::{AsContextMut, Caller, Global, Linker, Store, Trap, Val};

use std::convert::TryFrom;

fn process_err<S: SystemApi>(
    mut store: impl AsContextMut<Data = StoreData<S>>,
    e: HypervisorError,
) -> wasmtime::Trap {
    let t = wasmtime::Trap::new(format! {"{}", e});
    store
        .as_context_mut()
        .data_mut()
        .system_api
        .set_execution_error(e);
    t
}

/// Gets the global variable that stores the number of instructions from `caller`.
#[inline(always)]
fn get_num_instructions_global<S: SystemApi>(
    caller: &mut Caller<'_, StoreData<S>>,
    log: &ReplicaLogger,
    canister_id: CanisterId,
) -> Result<Global, Trap> {
    match caller.data().num_instructions_global {
        None => {
            error!(
                log,
                "[EXC-BUG] Canister {}: instructions counter is set to None.", canister_id,
            );
            Err(process_err(
                caller,
                HypervisorError::InstructionLimitExceeded,
            ))
        }
        Some(global) => Ok(global),
    }
}

#[inline(always)]
fn load_value<S: SystemApi>(
    global: &Global,
    mut caller: &mut Caller<'_, StoreData<S>>,
    log: &ReplicaLogger,
    canister_id: CanisterId,
) -> Result<NumInstructions, Trap> {
    match global.get(&mut caller) {
        Val::I64(instructions) => Ok(NumInstructions::from(instructions.max(0) as u64)),
        others => {
            error!(
                log,
                "[EXC-BUG] Canister {}: expected value of type I64 instead got {:?}",
                canister_id,
                others,
            );
            Err(process_err(
                caller,
                HypervisorError::InstructionLimitExceeded,
            ))
        }
    }
}

#[inline(always)]
fn store_value<S: SystemApi>(
    global: &Global,
    num_instructions: NumInstructions,
    mut caller: &mut Caller<'_, StoreData<S>>,
    log: &ReplicaLogger,
    canister_id: CanisterId,
) -> Result<(), Trap> {
    if let Err(err) = global.set(&mut caller, Val::I64(num_instructions.get() as i64)) {
        error!(
            log,
            "[EXC-BUG] Canister {}: Setting instructions to {} failed with {}",
            canister_id,
            num_instructions,
            err
        );
        return Err(process_err(
            caller,
            HypervisorError::InstructionLimitExceeded,
        ));
    }
    Ok(())
}

/// Charges a canister (in instructions) for system API call overhead (exit,
/// accessing state, etc) and for using `num_bytes` bytes of memory. If
/// the canister has run out instructions or there are unexpected bugs, return
/// an error.
///
/// There are a number of scenarios that this function must handle where due
/// to potential bugs, the expected information is not available. In more
/// classical systems, we could just panic in such cases. However, for us
/// that has the danger of putting the subnet in a crash loop. So instead,
/// we emit a error log message and continue execution. We intentionally do
/// not introduce new error types in these paths as these error paths should
/// be extremely rare and we do not want to increase the complexity of the
/// code to handle hypothetical bugs.
//
// Note: marked not for inlining as we don't want to spill this code into every system API call.
#[inline(never)]
fn charge_for_system_api_call<S: SystemApi>(
    log: &ReplicaLogger,
    canister_id: CanisterId,
    caller: &mut Caller<'_, StoreData<S>>,
    system_api_overhead: NumInstructions,
    num_bytes: u32,
    complexity: &ExecutionComplexity,
) -> Result<(), Trap> {
    observe_execution_complexity(log, canister_id, caller, complexity)?;
    let num_instructions_global = get_num_instructions_global(caller, log, canister_id)?;
    let current_instructions =
        load_value(&num_instructions_global, caller, log, canister_id)?.get() as i64;
    // Assert the current instruction counter is sane
    let system_api = &caller.data().system_api;
    let instruction_limit = system_api.slice_instruction_limit().get() as i64;
    if current_instructions > instruction_limit {
        error!(
            log,
            "[EXC-BUG] Canister {}: current instructions counter {} is greater than the limit {}",
            canister_id,
            current_instructions,
            instruction_limit
        );
        // Continue execution
    }
    let fee = system_api
        .get_num_instructions_from_bytes(NumBytes::from(num_bytes as u64))
        .get() as i64
        + system_api_overhead.get() as i64;
    if current_instructions < fee {
        info!(
            log,
            "Canister {}: ran out of instructions.  Current {}, fee {}",
            canister_id,
            current_instructions,
            fee
        );
        return Err(process_err(
            caller,
            HypervisorError::InstructionLimitExceeded,
        ));
    }
    let updated_instructions = NumInstructions::from((current_instructions - fee) as u64);
    store_value(
        &num_instructions_global,
        updated_instructions,
        caller,
        log,
        canister_id,
    )?;
    Ok(())
}

/// Observe execution complexity.
fn observe_execution_complexity<S: SystemApi>(
    log: &ReplicaLogger,
    canister_id: CanisterId,
    caller: &mut Caller<'_, StoreData<S>>,
    complexity: &ExecutionComplexity,
) -> Result<(), Trap> {
    let system_api = &mut caller.data_mut().system_api;
    let total_complexity = system_api.get_total_execution_complexity() + complexity;
    if system_api.subnet_type() != SubnetType::System {
        // TODO: RUN-126: Implement per-round complexity that combines complexities of
        //       multiple messages.
        // Note: for install messages the CPU Limit will be > 1s, but it will be addressed with DTS
        let total_instruction_limit = system_api.total_instruction_limit();
        if total_complexity.cpu > total_instruction_limit {
            error!(
                log,
                "Canister {}: Error exceeding CPU complexity limit: (observed:{}, limit:{})",
                canister_id,
                total_complexity.cpu,
                total_instruction_limit,
            );
            return Err(process_err(
                caller,
                HypervisorError::InstructionLimitExceeded,
            ));
        }
    }
    system_api.set_total_execution_complexity(total_complexity);
    Ok(())
}

/// A helper to pass wasmtime counters to the System API
fn ic0_performance_counter_helper<S: SystemApi>(
    log: &ReplicaLogger,
    canister_id: CanisterId,
    caller: &mut Caller<'_, StoreData<S>>,
    counter_type: u32,
) -> Result<u64, Trap> {
    let performance_counter_type = match counter_type {
        0 => {
            let num_instructions_global = get_num_instructions_global(caller, log, canister_id)?;
            let current_instructions =
                load_value(&num_instructions_global, caller, log, canister_id)?.get();

            let instructions_limit = caller.data().system_api.total_instruction_limit().get();
            let instructions_used = instructions_limit
                .checked_sub(current_instructions)
                .unwrap_or(instructions_limit);

            PerformanceCounterType::Instructions(instructions_used.into())
        }
        _ => {
            return Err(process_err(
                caller,
                HypervisorError::ContractViolation(format!(
                    "Error getting performance counter type {}",
                    counter_type
                )),
            ));
        }
    };
    caller
        .data()
        .system_api
        .ic0_performance_counter(performance_counter_type)
        .map_err(|e| process_err(caller, e))
}

pub(crate) fn syscalls<S: SystemApi>(
    log: ReplicaLogger,
    canister_id: CanisterId,
    store: &Store<StoreData<S>>,
    rate_limiting_of_debug_prints: FlagStatus,
) -> Linker<StoreData<S>> {
    fn with_system_api<S, T>(caller: &mut Caller<'_, StoreData<S>>, f: impl Fn(&mut S) -> T) -> T {
        f(&mut caller.as_context_mut().data_mut().system_api)
    }

    fn with_memory_and_system_api<S: SystemApi, T>(
        mut caller: Caller<'_, StoreData<S>>,
        f: impl Fn(&mut S, &mut [u8]) -> HypervisorResult<T>,
    ) -> Result<T, wasmtime::Trap> {
        let result = caller
            .get_export("memory")
            .ok_or_else(|| {
                HypervisorError::ContractViolation(
                    "WebAssembly module must define memory".to_string(),
                )
            })
            .and_then(|ext| {
                ext.into_memory().ok_or_else(|| {
                    HypervisorError::ContractViolation(
                        "export 'memory' is not a memory".to_string(),
                    )
                })
            })
            .and_then(|mem| {
                let mem = mem.data_mut(&mut caller);
                let ptr = mem.as_mut_ptr();
                let len = mem.len();
                // SAFETY: The memory array is valid for the duration of our borrow of the
                // `SystemApi` and the mutating the `SystemApi` cannot change the memory array
                // so it's safe to mutate both at once.  If the memory and system_api were two
                // fields of the `caller` struct then this would be allowed, but
                // since we access them through opaque functions the
                // compiler can't know that they are unrelated objects.
                f(&mut caller.as_context_mut().data_mut().system_api, unsafe {
                    std::slice::from_raw_parts_mut(ptr, len)
                })
            });
        match result {
            Err(e) => Err(process_err(caller, e)),
            Ok(r) => Ok(r),
        }
    }

    let mut linker = Linker::new(store.engine());

    linker
        .func_wrap("ic0", "msg_caller_copy", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: i32, offset: i32, size: i32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_CALLER_COPY,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_msg_caller_copy(dst as u32, offset as u32, size as u32, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_caller_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_msg_caller_size())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i32::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0::msg_caller_size failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_arg_data_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_msg_arg_data_size())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i32::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0::msg_arg_data_size failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_arg_data_copy", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: i32, offset: i32, size: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::MSG_ARG_DATA_COPY,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_ARG_DATA_COPY,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, mem| {
                    system_api.ic0_msg_arg_data_copy(dst as u32, offset as u32, size as u32, mem)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_method_name_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_msg_method_name_size())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i32::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0::msg_metohd_name_size failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_method_name_copy", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: i32, offset: i32, size: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::MSG_METHOD_NAME_COPY,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_METHOD_NAME_COPY,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_msg_method_name_copy(
                        dst as u32,
                        offset as u32,
                        size as u32,
                        memory,
                    )
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "accept_message", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_accept_message())
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_reply_data_append", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, src: i32, size: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::MSG_REPLY_DATA_APPEND,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_REPLY_DATA_APPEND,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: (size as u64).into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_msg_reply_data_append(src as u32, size as u32, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_reply", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_msg_reply())
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_reject_code", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_msg_reject_code())
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_reject", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, src: i32, size: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::MSG_REJECT,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_REJECT,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: (size as u64).into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_msg_reject(src as u32, size as u32, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_reject_msg_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_msg_reject_msg_size())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i32::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_msg_reject_msg_size failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_reject_msg_copy", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: i32, offset: i32, size: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::MSG_REJECT_MSG_COPY,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_REJECT_MSG_COPY,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_msg_reject_msg_copy(
                        dst as u32,
                        offset as u32,
                        size as u32,
                        memory,
                    )
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "canister_self_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_canister_self_size())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i32::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_canister_self_size failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "canister_self_copy", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: i32, offset: i32, size: i32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::CANISTER_SELF_COPY,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_canister_self_copy(
                        dst as u32,
                        offset as u32,
                        size as u32,
                        memory,
                    )
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "controller_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_controller_size())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i32::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_controller_size failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "controller_copy", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: i32, offset: i32, size: i32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::CONTROLLER_COPY,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_controller_copy(dst as u32, offset as u32, size as u32, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "debug_print", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, offset: i32, length: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::DEBUG_PRINT,
                    length as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::DEBUG_PRINT,
                        memory: (length as u64).into(),
                        disk: (length as u64).into(),
                        network: (length as u64).into(),
                    },
                )?;
                match (
                    caller.data().system_api.subnet_type(),
                    rate_limiting_of_debug_prints,
                ) {
                    // Debug print is a no-op on non-system subnets with rate limiting.
                    (SubnetType::Application, FlagStatus::Enabled) => Ok(()),
                    (SubnetType::VerifiedApplication, FlagStatus::Enabled) => Ok(()),
                    // If rate limiting is disabled or the subnet is a system subnet, then
                    // debug print produces output.
                    (_, FlagStatus::Disabled) | (SubnetType::System, FlagStatus::Enabled) => {
                        with_memory_and_system_api(caller, |system_api, memory| {
                            system_api.ic0_debug_print(offset as u32, length as u32, memory)
                        })
                    }
                }
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "trap", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, offset: i32, length: i32| -> Result<(), _> {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::TRAP,
                    length as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::TRAP,
                        memory: (length as u64).into(),
                        disk: (length as u64).into(),
                        network: (length as u64).into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_trap(offset as u32, length as u32, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "call_simple", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>,
                  callee_src: i32,
                  callee_size: i32,
                  name_src: i32,
                  name_len: i32,
                  reply_fun: i32,
                  reply_env: i32,
                  reject_fun: i32,
                  reject_env: i32,
                  src: i32,
                  len: i32| {
                let total_len = callee_size as u64 + name_len as u64 + len as u64;
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::CALL_SIMPLE,
                    len as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::CALL_SIMPLE,
                        memory: (total_len as u64).into(),
                        disk: 0.into(),
                        network: (total_len as u64).into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_call_simple(
                        callee_src as u32,
                        callee_size as u32,
                        name_src as u32,
                        name_len as u32,
                        reply_fun as u32,
                        reply_env as u32,
                        reject_fun as u32,
                        reject_env as u32,
                        src as u32,
                        len as u32,
                        memory,
                    )
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "call_new", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>,
                  callee_src: i32,
                  callee_size: i32,
                  name_src: i32,
                  name_len: i32,
                  reply_fun: i32,
                  reply_env: i32,
                  reject_fun: i32,
                  reject_env: i32| {
                let total_len = callee_size as u64 + name_len as u64;
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::CALL_NEW,
                        memory: (total_len).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_call_new(
                        callee_src as u32,
                        callee_size as u32,
                        name_src as u32,
                        name_len as u32,
                        reply_fun as u32,
                        reply_env as u32,
                        reject_fun as u32,
                        reject_env as u32,
                        memory,
                    )
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "call_data_append", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, src: i32, size: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::CALL_DATA_APPEND,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::CALL_DATA_APPEND,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: (size as u64).into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_call_data_append(src as u32, size as u32, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "call_on_cleanup", {
            move |mut caller: Caller<'_, StoreData<S>>, fun: i32, env: i32| {
                with_system_api(&mut caller, |s| {
                    s.ic0_call_on_cleanup(fun as u32, env as u32)
                })
                .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "call_cycles_add", {
            move |mut caller: Caller<'_, StoreData<S>>, amount: i64| {
                with_system_api(&mut caller, |s| s.ic0_call_cycles_add(amount as u64))
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "call_cycles_add128", {
            move |mut caller: Caller<'_, StoreData<S>>, amount_high: i64, amount_low: i64| {
                with_system_api(&mut caller, |s| {
                    s.ic0_call_cycles_add128(Cycles::from_parts(
                        amount_high as u64,
                        amount_low as u64,
                    ))
                })
                .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "call_perform", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::CALL_PERFORM,
                        memory: 0.into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_system_api(&mut caller, |s| s.ic0_call_perform())
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "stable_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_stable_size())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i32::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_stable_size failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "stable_grow", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, additional_pages: i32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::STABLE_GROW,
                        memory: 0.into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_system_api(&mut caller, |s| s.ic0_stable_grow(additional_pages as u32))
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "stable_read", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: i32, offset: i32, size: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::STABLE_READ,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::STABLE_READ,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_stable_read(dst as u32, offset as u32, size as u32, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "stable_write", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, offset: i32, src: i32, size: i32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::STABLE_WRITE,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::STABLE_WRITE,
                        memory: (size as u64).into(),
                        disk: (size as u64).into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_stable_write(offset as u32, src as u32, size as u32, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "stable64_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_stable64_size())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i64::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_stable64_size failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "stable64_grow", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, additional_pages: i64| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::STABLE64_GROW,
                        memory: 0.into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_system_api(&mut caller, |s| {
                    s.ic0_stable64_grow(additional_pages as u64)
                })
                .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "stable64_read", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: i64, offset: i64, size: i64| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::STABLE64_READ,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::STABLE64_READ,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_stable64_read(dst as u64, offset as u64, size as u64, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "stable64_write", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, offset: i64, src: i64, size: i64| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::STABLE64_WRITE,
                    size as u32,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::STABLE64_WRITE,
                        memory: (size as u64).into(),
                        disk: (size as u64).into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_stable64_write(offset as u64, src as u64, size as u64, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "time", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_time())
                    .map_err(|e| process_err(caller, e))
                    .map(|s| s.as_nanos_since_unix_epoch())
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "performance_counter", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, counter_type: u32| {
                charge_for_system_api_call(
                    &log,
                    canister_id,
                    &mut caller,
                    system_api_complexity::overhead::PERFORMANCE_COUNTER,
                    0,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::PERFORMANCE_COUNTER,
                        memory: 0.into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                ic0_performance_counter_helper(&log, canister_id, &mut caller, counter_type)
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "canister_cycle_balance", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_canister_cycle_balance())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i64::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_canister_cycle_balance failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "canister_cycle_balance128", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: u32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::CANISTER_CYCLES_BALANCE128,
                        memory: (std::mem::size_of::<u64>() as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_canister_cycles_balance128(dst, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_cycles_available", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_msg_cycles_available())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i64::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_msg_cycles_available failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_cycles_available128", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: u32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_CYCLES_AVAILABLE128,
                        memory: (std::mem::size_of::<u64>() as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_msg_cycles_available128(dst, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_cycles_refunded", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_msg_cycles_refunded())
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i64::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_msg_cycles_refunded failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_cycles_refunded128", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, dst: u32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_CYCLES_REFUNDED128,
                        memory: (std::mem::size_of::<u64>() as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_msg_cycles_refunded128(dst, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_cycles_accept", {
            move |mut caller: Caller<'_, StoreData<S>>, amount: i64| {
                with_system_api(&mut caller, |s| s.ic0_msg_cycles_accept(amount as u64))
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "msg_cycles_accept128", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>,
                  amount_high: i64,
                  amount_low: i64,
                  dst: u32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::MSG_CYCLES_ACCEPT128,
                        memory: (std::mem::size_of::<u64>() as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_msg_cycles_accept128(
                        Cycles::from_parts(amount_high as u64, amount_low as u64),
                        dst,
                        memory,
                    )
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("__", "out_of_instructions", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>| -> Result<(), _> {
                let num_instructions_global =
                    get_num_instructions_global(&mut caller, &log, canister_id)?;
                let num_instructions_left =
                    load_value(&num_instructions_global, &mut caller, &log, canister_id)?;
                let result = with_system_api(&mut caller, |s| {
                    if num_instructions_left > s.slice_instruction_limit() {
                        error!(
                            log,
                            "[EXC-BUG] Canister {}: current instructions counter {} is greater than the limit {}",
                            canister_id,
                            num_instructions_left,
                            s.slice_instruction_limit(),
                        );
                    }
                    // The out-of-instruction handler expects that the number of
                    // left instructions does not exceed the limit.
                    s.out_of_instructions(num_instructions_left.min(s.slice_instruction_limit()))
                });

                match result {
                    Ok(updated_instructions) => {
                        store_value(
                            &num_instructions_global,
                            updated_instructions,
                            &mut caller,
                            &log,
                            canister_id,
                        )?;
                        Ok(())
                    }
                    Err(err) => Err(process_err(caller, err)),
                }
            }
        })
        .unwrap();

    linker
        .func_wrap("__", "update_available_memory", {
            move |mut caller: Caller<'_, StoreData<S>>,
                  native_memory_grow_res: i32,
                  additional_pages: i32| {
                with_system_api(&mut caller, |s| {
                    s.update_available_memory(native_memory_grow_res, additional_pages as u32)
                })
                .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "canister_status", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_canister_status())
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "certified_data_set", {
            let log = log.clone();
            move |mut caller: Caller<'_, StoreData<S>>, src: u32, size: u32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::CERTIFIED_DATA_SET,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_certified_data_set(src, size, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "data_certificate_present", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_data_certificate_present())
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "data_certificate_size", {
            move |mut caller: Caller<'_, StoreData<S>>| {
                with_system_api(&mut caller, |s| s.ic0_data_certificate_size())
                    .map_err(|e| process_err(caller, e))
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "data_certificate_copy", {
            move |mut caller: Caller<'_, StoreData<S>>, dst: u32, offset: u32, size: u32| {
                observe_execution_complexity(
                    &log,
                    canister_id,
                    &mut caller,
                    &ExecutionComplexity {
                        cpu: system_api_complexity::cpu::DATA_CERTIFICATE_COPY,
                        memory: (size as u64).into(),
                        disk: 0.into(),
                        network: 0.into(),
                    },
                )?;
                with_memory_and_system_api(caller, |system_api, memory| {
                    system_api.ic0_data_certificate_copy(dst, offset, size, memory)
                })
            }
        })
        .unwrap();

    linker
        .func_wrap("ic0", "mint_cycles", {
            move |mut caller: Caller<'_, StoreData<S>>, amount: i64| {
                with_system_api(&mut caller, |s| s.ic0_mint_cycles(amount as u64))
                    .map_err(|e| process_err(caller, e))
                    .and_then(|s| {
                        i64::try_from(s).map_err(|e| {
                            wasmtime::Trap::new(format!("ic0_mint_cycles failed: {}", e))
                        })
                    })
            }
        })
        .unwrap();

    linker
}
