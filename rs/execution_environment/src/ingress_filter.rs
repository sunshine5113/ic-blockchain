use crate::ExecutionEnvironmentImpl;
use ic_error_types::UserError;
use ic_interfaces::execution_environment::{ExecutionMode, IngressFilterService};
use ic_interfaces_state_manager::StateReader;
use ic_registry_provisional_whitelist::ProvisionalWhitelist;
use ic_replicated_state::ReplicatedState;
use ic_types::messages::SignedIngressContent;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::sync::oneshot;
use tower::{limit::GlobalConcurrencyLimitLayer, util::BoxCloneService, Service, ServiceBuilder};

#[derive(Clone)]
pub(crate) struct IngressFilter {
    exec_env: Arc<ExecutionEnvironmentImpl>,
    state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
    threadpool: Arc<Mutex<threadpool::ThreadPool>>,
}

impl IngressFilter {
    pub(crate) fn new_service(
        concurrency_buffer: GlobalConcurrencyLimitLayer,
        threadpool: Arc<Mutex<threadpool::ThreadPool>>,
        state_reader: Arc<dyn StateReader<State = ReplicatedState>>,
        exec_env: Arc<ExecutionEnvironmentImpl>,
    ) -> IngressFilterService {
        let base_service = BoxCloneService::new(Self {
            exec_env,
            state_reader,
            threadpool,
        });
        ServiceBuilder::new()
            .layer(concurrency_buffer)
            .service(base_service)
    }
}

impl Service<(ProvisionalWhitelist, SignedIngressContent)> for IngressFilter {
    type Response = Result<(), UserError>;
    type Error = Infallible;
    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(
        &mut self,
        (provisional_whitelist, ingress): (ProvisionalWhitelist, SignedIngressContent),
    ) -> Self::Future {
        let exec_env = Arc::clone(&self.exec_env);
        let state_reader = Arc::clone(&self.state_reader);
        let (tx, rx) = oneshot::channel();
        let threadpool = self.threadpool.lock().unwrap().clone();
        threadpool.execute(move || {
            if !tx.is_closed() {
                let state = state_reader.get_latest_state().take();
                let v = exec_env.should_accept_ingress_message(
                    state,
                    &provisional_whitelist,
                    &ingress,
                    ExecutionMode::NonReplicated,
                );
                let _ = tx.send(Ok(v));
            }
        });
        Box::pin(async move {
            rx.await
                .expect("The sender was dropped before sending the message.")
        })
    }
}
