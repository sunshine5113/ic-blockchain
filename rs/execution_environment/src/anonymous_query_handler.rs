use crate::ExecutionEnvironmentImpl;
use ic_error_types::RejectCode;
use ic_interfaces::execution_environment::AnonymousQueryService;
use ic_interfaces_state_manager::StateReader;
use ic_replicated_state::ReplicatedState;
use ic_types::{
    ingress::WasmResult,
    messages::{AnonymousQuery, AnonymousQueryResponse, AnonymousQueryResponseReply, Blob},
    NumInstructions,
};
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::oneshot;
use tower::{limit::GlobalConcurrencyLimitLayer, util::BoxCloneService, Service, ServiceBuilder};

#[derive(Clone)]
// Struct that is responsible for handling queries sent by internal IC components.
pub(crate) struct AnonymousQueryHandler {
    exec_env: Arc<ExecutionEnvironmentImpl>,
    state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
    threadpool: Arc<Mutex<threadpool::ThreadPool>>,
    max_instructions_per_message: NumInstructions,
}

impl AnonymousQueryHandler {
    pub(crate) fn new_service(
        concurrency_buffer: GlobalConcurrencyLimitLayer,
        threadpool: Arc<Mutex<threadpool::ThreadPool>>,
        state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
        exec_env: Arc<ExecutionEnvironmentImpl>,
        max_instructions_per_message: NumInstructions,
    ) -> AnonymousQueryService {
        let base_service = BoxCloneService::new(Self {
            exec_env,
            state_reader,
            threadpool,
            max_instructions_per_message,
        });
        ServiceBuilder::new()
            .layer(concurrency_buffer)
            .service(base_service)
    }
}

impl Service<AnonymousQuery> for AnonymousQueryHandler {
    type Response = AnonymousQueryResponse;
    type Error = Infallible;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, anonymous_query: AnonymousQuery) -> Self::Future {
        let instructions_limit = self.max_instructions_per_message;
        let exec_env = Arc::clone(&self.exec_env);
        let state_reader = Arc::clone(&self.state_reader);
        let (tx, rx) = oneshot::channel();
        let threadpool = self.threadpool.lock().unwrap().clone();
        threadpool.execute(move || {
            if !tx.is_closed() {
                let state = state_reader.get_latest_state().take();
                let result =
                    exec_env.execute_anonymous_query(anonymous_query, state, instructions_limit);

                let anonymous_query_response = match result {
                    Ok(wasm_result) => match wasm_result {
                        WasmResult::Reply(vec) => AnonymousQueryResponse::Replied {
                            reply: AnonymousQueryResponseReply { arg: Blob(vec) },
                        },
                        WasmResult::Reject(message) => AnonymousQueryResponse::Rejected {
                            reject_code: RejectCode::CanisterReject,
                            reject_message: message,
                        },
                    },
                    Err(user_error) => AnonymousQueryResponse::Rejected {
                        reject_code: user_error.reject_code(),
                        reject_message: user_error.to_string(),
                    },
                };

                let _ = tx.send(Ok(anonymous_query_response));
            }
        });
        Box::pin(async move {
            rx.await
                .expect("The sender was dropped before sending the message.")
        })
    }
}
