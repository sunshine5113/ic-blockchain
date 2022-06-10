#[cfg(test)]
mod canister_state {
    use ic_base_types::NumBytes;
    use ic_registry_subnet_type::SubnetType;
    use ic_replicated_state::{
        testing::{CanisterQueuesTesting, SystemStateTesting},
        InputQueueType, StateError,
    };
    use ic_test_utilities::state::{
        get_running_canister, get_stopped_canister, get_stopping_canister,
    };
    use ic_test_utilities::types::ids::canister_test_id;
    use ic_test_utilities::types::messages::{RequestBuilder, ResponseBuilder};
    use ic_types::messages::RequestOrResponse;
    use ic_types::QueueIndex;

    const MAX_CANISTER_MEMORY_SIZE: NumBytes = NumBytes::new(u64::MAX / 2);
    const SUBNET_AVAILABLE_MEMORY: i64 = i64::MAX / 2;

    #[test]
    fn running_canister_accepts_requests() {
        let mut canister = get_running_canister(canister_test_id(0));

        assert_eq!(
            canister.system_state.queues_mut().push_input(
                QueueIndex::new(0),
                RequestBuilder::new().build().into(),
                InputQueueType::RemoteSubnet,
            ),
            Ok(())
        );
    }

    #[test]
    fn running_canister_accepts_responses() {
        let mut canister = get_running_canister(canister_test_id(0));

        assert_eq!(
            canister.push_output_request(
                RequestBuilder::new()
                    .sender(canister_test_id(0))
                    .receiver(canister_test_id(1))
                    .build()
                    .into(),
            ),
            Ok(())
        );

        assert_eq!(
            canister.system_state.queues_mut().push_input(
                QueueIndex::new(0),
                ResponseBuilder::new()
                    .originator(canister_test_id(0))
                    .respondent(canister_test_id(1))
                    .build()
                    .into(),
                InputQueueType::RemoteSubnet,
            ),
            Ok(())
        );
    }

    #[test]
    fn stopping_canister_rejects_requests() {
        let mut canister = get_stopping_canister(canister_test_id(0));

        let request: RequestOrResponse = RequestBuilder::new().build().into();
        assert_eq!(
            canister.push_input(
                QueueIndex::new(0),
                request.clone(),
                MAX_CANISTER_MEMORY_SIZE,
                &mut SUBNET_AVAILABLE_MEMORY.clone(),
                SubnetType::Application,
                InputQueueType::RemoteSubnet,
            ),
            Err((StateError::CanisterStopping(canister_test_id(0)), request))
        );
    }

    #[test]
    fn stopping_canister_accepts_responses() {
        let mut canister = get_stopping_canister(canister_test_id(0));

        assert_eq!(
            canister.push_output_request(
                RequestBuilder::new()
                    .sender(canister_test_id(0))
                    .receiver(canister_test_id(1))
                    .build()
                    .into(),
            ),
            Ok(())
        );

        let response: RequestOrResponse = ResponseBuilder::new()
            .originator(canister_test_id(0))
            .respondent(canister_test_id(1))
            .build()
            .into();
        assert_eq!(
            canister.system_state.queues_mut().push_input(
                QueueIndex::new(0),
                response,
                InputQueueType::RemoteSubnet,
            ),
            Ok(())
        );
    }

    #[test]
    fn stopped_canister_rejects_requests() {
        let mut canister = get_stopped_canister(canister_test_id(0));

        let request: RequestOrResponse = RequestBuilder::new().build().into();
        assert_eq!(
            canister.push_input(
                QueueIndex::new(0),
                request.clone(),
                MAX_CANISTER_MEMORY_SIZE,
                &mut SUBNET_AVAILABLE_MEMORY.clone(),
                SubnetType::Application,
                InputQueueType::RemoteSubnet,
            ),
            Err((StateError::CanisterStopped(canister_test_id(0)), request))
        );
    }

    #[test]
    fn stopped_canister_rejects_responses() {
        let mut canister = get_stopped_canister(canister_test_id(0));

        assert_eq!(
            canister.push_output_request(
                RequestBuilder::new()
                    .sender(canister_test_id(0))
                    .receiver(canister_test_id(1))
                    .build()
                    .into(),
            ),
            Ok(())
        );

        let response: RequestOrResponse = ResponseBuilder::new()
            .originator(canister_test_id(0))
            .respondent(canister_test_id(1))
            .build()
            .into();
        assert_eq!(
            canister.push_input(
                QueueIndex::new(0),
                response.clone(),
                MAX_CANISTER_MEMORY_SIZE,
                &mut SUBNET_AVAILABLE_MEMORY.clone(),
                SubnetType::Application,
                InputQueueType::RemoteSubnet,
            ),
            Err((StateError::CanisterStopped(canister_test_id(0)), response))
        );
    }
}
