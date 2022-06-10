use ic_base_types::{CanisterId, NumBytes, NumSeconds, PrincipalId, SubnetId};
use ic_btc_types_internal::{
    BitcoinAdapterRequestWrapper, BitcoinAdapterResponse, BitcoinAdapterResponseWrapper,
    GetSuccessorsRequest, GetSuccessorsResponse,
};
use ic_registry_subnet_features::SubnetFeatures;
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::replicated_state::testing::ReplicatedStateTesting;
use ic_replicated_state::testing::{CanisterQueuesTesting, SystemStateTesting};
use ic_replicated_state::{
    replicated_state::PeekableOutputIterator, replicated_state::ReplicatedStateMessageRouting,
    BitcoinStateError, CanisterState, InputQueueType, ReplicatedState, SchedulerState, StateError,
    SystemState,
};
use ic_test_utilities::state::{
    arb_replicated_state_with_queues, get_running_canister, register_callback,
};
use ic_test_utilities::types::ids::canister_test_id;
use ic_test_utilities::types::{
    ids::{subnet_test_id, user_test_id},
    messages::{RequestBuilder, ResponseBuilder},
};
use ic_types::{
    messages::{CallbackId, RequestOrResponse, MAX_RESPONSE_COUNT_BYTES},
    CountBytes, Cycles, QueueIndex,
};
use proptest::prelude::*;
use std::str::FromStr;

const SUBNET_ID: SubnetId = SubnetId::new(PrincipalId::new(29, [0xfc; 29]));
const CANISTER_ID: CanisterId = CanisterId::from_u64(42);
const OTHER_CANISTER_ID: CanisterId = CanisterId::from_u64(13);

const INITIAL_CYCLES: Cycles = Cycles::new(1 << 36);

const MAX_CANISTER_MEMORY_SIZE: NumBytes = NumBytes::new(u64::MAX / 2);
const SUBNET_AVAILABLE_MEMORY: i64 = i64::MAX / 2;

fn replicated_state_test<F, R>(f: F) -> R
where
    F: FnOnce(ReplicatedState) -> R,
{
    let scheduler_state = SchedulerState::default();
    let system_state = SystemState::new_running(
        CANISTER_ID,
        user_test_id(24).get(),
        INITIAL_CYCLES,
        NumSeconds::from(100_000),
    );
    let canister_state = CanisterState::new(system_state, None, scheduler_state);
    let mut state =
        ReplicatedState::new_rooted_at(SUBNET_ID, SubnetType::Application, "unused".into());
    state.put_canister_state(canister_state);

    f(state)
}

fn assert_total_memory_taken(queues_memory_usage: usize, state: &ReplicatedState) {
    assert_eq!(queues_memory_usage as u64, state.total_memory_taken().get());
}

fn assert_total_memory_taken_with_messages(queues_memory_usage: usize, state: &ReplicatedState) {
    assert_eq!(
        queues_memory_usage as u64,
        state.total_memory_taken_with_messages().get()
    );
}

fn assert_message_memory_taken(queues_memory_usage: usize, state: &ReplicatedState) {
    assert_eq!(
        queues_memory_usage as u64,
        state.message_memory_taken().get()
    );
}

fn assert_subnet_available_memory(
    initial_available_memory: i64,
    queues_memory_usage: usize,
    actual: i64,
) {
    assert_eq!(
        initial_available_memory - queues_memory_usage as i64,
        actual
    );
}

#[test]
fn total_memory_taken_by_canister_queues() {
    replicated_state_test(|mut state| {
        let mut subnet_available_memory = SUBNET_AVAILABLE_MEMORY;

        // Zero memory used initially.
        assert_eq!(0, state.total_memory_taken().get());

        // Push a request into a canister input queue.
        state
            .push_input(
                QueueIndex::from(0),
                RequestBuilder::default()
                    .sender(OTHER_CANISTER_ID)
                    .receiver(CANISTER_ID)
                    .build()
                    .into(),
                MAX_CANISTER_MEMORY_SIZE,
                &mut subnet_available_memory,
            )
            .unwrap();

        // Reserved memory for one response.
        assert_total_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_total_memory_taken_with_messages(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_subnet_available_memory(
            SUBNET_AVAILABLE_MEMORY,
            MAX_RESPONSE_COUNT_BYTES,
            subnet_available_memory,
        );

        assert!(state
            .canister_state_mut(&CANISTER_ID)
            .unwrap()
            .pop_input()
            .is_some());

        // Unchanged memory usage.
        assert_total_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_total_memory_taken_with_messages(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);

        // Push a response into the output queue.
        let response = ResponseBuilder::default()
            .respondent(CANISTER_ID)
            .originator(OTHER_CANISTER_ID)
            .build();
        state
            .canister_state_mut(&CANISTER_ID)
            .unwrap()
            .push_output_response(response.clone().into());

        // Memory used by response only.
        assert_total_memory_taken(response.count_bytes(), &state);
        assert_total_memory_taken_with_messages(response.count_bytes(), &state);
        assert_message_memory_taken(response.count_bytes(), &state);
    })
}

#[test]
fn total_memory_taken_by_subnet_queues() {
    replicated_state_test(|mut state| {
        let mut subnet_available_memory = SUBNET_AVAILABLE_MEMORY;

        // Zero memory used initially.
        assert_eq!(0, state.total_memory_taken().get());

        // Push a request into the subnet input queues. Should ignore the
        // `max_canister_memory_size` argument.
        state
            .push_input(
                QueueIndex::from(0),
                RequestBuilder::default()
                    .sender(CANISTER_ID)
                    .receiver(SUBNET_ID.into())
                    .build()
                    .into(),
                0.into(),
                &mut subnet_available_memory,
            )
            .unwrap();

        // Reserved memory for one response.
        assert_total_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_total_memory_taken_with_messages(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_subnet_available_memory(
            SUBNET_AVAILABLE_MEMORY,
            MAX_RESPONSE_COUNT_BYTES,
            subnet_available_memory,
        );

        assert!(state.pop_subnet_input().is_some());

        // Unchanged memory usage.
        assert_total_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_total_memory_taken_with_messages(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);

        // Push a response into the subnet output queues.
        let response = ResponseBuilder::default()
            .respondent(SUBNET_ID.into())
            .originator(CANISTER_ID)
            .build();
        state.push_subnet_output_response(response.clone().into());

        // Memory used by response only.
        assert_total_memory_taken(response.count_bytes(), &state);
        assert_total_memory_taken_with_messages(response.count_bytes(), &state);
        assert_message_memory_taken(response.count_bytes(), &state);
    })
}

#[test]
fn total_memory_taken_by_stream_responses() {
    replicated_state_test(|mut state| {
        // Zero memory used initially.
        assert_eq!(0, state.total_memory_taken().get());

        // Push a request and a response into a stream.
        let mut streams = state.take_streams();
        streams.push(
            SUBNET_ID,
            RequestBuilder::default()
                .sender(CANISTER_ID)
                .receiver(OTHER_CANISTER_ID)
                .build()
                .into(),
        );
        let response: RequestOrResponse = ResponseBuilder::default()
            .respondent(CANISTER_ID)
            .originator(OTHER_CANISTER_ID)
            .build()
            .into();
        streams.push(SUBNET_ID, response.clone());
        state.put_streams(streams);

        // Memory only used by response, not request.
        assert_total_memory_taken(response.count_bytes(), &state);
        assert_total_memory_taken_with_messages(response.count_bytes(), &state);
        assert_message_memory_taken(response.count_bytes(), &state);
    })
}

#[test]
fn system_subnet_total_memory_taken_by_canister_queues() {
    replicated_state_test(|mut state| {
        // Make it a system subnet.
        state.metadata.own_subnet_type = SubnetType::System;

        let mut subnet_available_memory = SUBNET_AVAILABLE_MEMORY;

        // Zero memory used initially.
        assert_eq!(0, state.total_memory_taken().get());

        // Push a request into a canister input queue.
        state
            .push_input(
                QueueIndex::from(0),
                RequestBuilder::default()
                    .sender(OTHER_CANISTER_ID)
                    .receiver(CANISTER_ID)
                    .build()
                    .into(),
                MAX_CANISTER_MEMORY_SIZE,
                &mut subnet_available_memory,
            )
            .unwrap();

        // System subnets don't account for messages in `total_memory_taken()`.
        assert_total_memory_taken(0, &state);
        // But do in other `memory_taken()` methods.
        assert_total_memory_taken_with_messages(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        // And `&mut subnet_available_memory` is updated by the push.
        assert_subnet_available_memory(
            SUBNET_AVAILABLE_MEMORY,
            MAX_RESPONSE_COUNT_BYTES,
            subnet_available_memory,
        );
    })
}

#[test]
fn system_subnet_total_memory_taken_by_subnet_queues() {
    replicated_state_test(|mut state| {
        // Make it a system subnet.
        state.metadata.own_subnet_type = SubnetType::System;

        let mut subnet_available_memory = SUBNET_AVAILABLE_MEMORY;

        // Zero memory used initially.
        assert_eq!(0, state.total_memory_taken().get());

        // Push a request into the subnet input queues. Should ignore the
        // `max_canister_memory_size` argument.
        state
            .push_input(
                QueueIndex::from(0),
                RequestBuilder::default()
                    .sender(CANISTER_ID)
                    .receiver(SUBNET_ID.into())
                    .build()
                    .into(),
                0.into(),
                &mut subnet_available_memory,
            )
            .unwrap();

        // System subnets don't account for subnet queue messages in `total_memory_taken()`.
        assert_total_memory_taken(0, &state);
        // But do in other `memory_taken()` methods.
        assert_total_memory_taken_with_messages(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        // And `&mut subnet_available_memory` is updated by the push.
        assert_subnet_available_memory(
            SUBNET_AVAILABLE_MEMORY,
            MAX_RESPONSE_COUNT_BYTES,
            subnet_available_memory,
        );
    })
}

#[test]
fn system_subnet_total_memory_taken_by_stream_responses() {
    replicated_state_test(|mut state| {
        // Make it a system subnet.
        state.metadata.own_subnet_type = SubnetType::System;

        // Zero memory used initially.
        assert_eq!(0, state.total_memory_taken().get());

        // Push a request and a response into a stream.
        let mut streams = state.take_streams();
        streams.push(
            SUBNET_ID,
            RequestBuilder::default()
                .sender(CANISTER_ID)
                .receiver(OTHER_CANISTER_ID)
                .build()
                .into(),
        );
        let response: RequestOrResponse = ResponseBuilder::default()
            .respondent(CANISTER_ID)
            .originator(OTHER_CANISTER_ID)
            .build()
            .into();
        streams.push(SUBNET_ID, response.clone());
        state.put_streams(streams);

        // System subnets don't account for stream responses in `total_memory_taken()`.
        assert_total_memory_taken(0, &state);
        // But do in other `memory_taken()` methods.
        assert_total_memory_taken_with_messages(response.count_bytes(), &state);
        assert_message_memory_taken(response.count_bytes(), &state);
    })
}

#[test]
fn push_subnet_queues_input_respects_subnet_available_memory() {
    replicated_state_test(|mut state| {
        let initial_available_memory = MAX_RESPONSE_COUNT_BYTES as i64;
        let mut subnet_available_memory = initial_available_memory;

        // Zero memory used initially.
        assert_eq!(0, state.total_memory_taken().get());

        // Push a request into the subnet input queues. Should ignore the
        // `max_canister_memory_size` argument.
        state
            .push_input(
                QueueIndex::from(0),
                RequestBuilder::default()
                    .sender(OTHER_CANISTER_ID)
                    .receiver(SUBNET_ID.into())
                    .build()
                    .into(),
                0.into(),
                &mut subnet_available_memory,
            )
            .unwrap();

        // Reserved memory for one response.
        assert_total_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_total_memory_taken_with_messages(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_message_memory_taken(MAX_RESPONSE_COUNT_BYTES, &state);
        assert_subnet_available_memory(
            initial_available_memory,
            MAX_RESPONSE_COUNT_BYTES,
            subnet_available_memory,
        );

        // Push a second request into the subnet input queues.
        let request: RequestOrResponse = RequestBuilder::default()
            .sender(CANISTER_ID)
            .receiver(SUBNET_ID.into())
            .build()
            .into();
        let res = state.push_input(
            QueueIndex::from(0),
            request.clone(),
            0.into(),
            &mut subnet_available_memory,
        );

        // No more memory for a second request.
        assert_eq!(
            Err((
                StateError::OutOfMemory {
                    requested: (MAX_RESPONSE_COUNT_BYTES as u64).into(),
                    available: 0.into()
                },
                request
            )),
            res
        );

        // Unchanged memory usage.
        assert_eq!(
            MAX_RESPONSE_COUNT_BYTES as u64,
            state.total_memory_taken().get()
        );
        assert_eq!(0, subnet_available_memory);
    })
}

#[test]
fn push_input_queues_respects_local_remote_subnet() {
    // Local and remote IDs.
    let local_canister_id = CanisterId::from_u64(1);
    let local_subnet_id = subnet_test_id(2);
    let remote_canister_id = CanisterId::from_u64(0x101);

    // Create replicated state.
    let scheduler_state = SchedulerState::default();
    let system_state = SystemState::new_running(
        local_canister_id,
        user_test_id(24).get(),
        INITIAL_CYCLES,
        NumSeconds::from(100_000),
    );
    let canister_state = CanisterState::new(system_state, None, scheduler_state);
    let mut state =
        ReplicatedState::new_rooted_at(local_subnet_id, SubnetType::Application, "unused".into());
    state.put_canister_state(canister_state);

    // Assert the queues are empty.
    assert_eq!(
        state
            .canister_state_mut(&local_canister_id)
            .unwrap()
            .system_state
            .queues_mut()
            .pop_input(),
        None
    );
    assert_eq!(state.canister_state(&remote_canister_id), None);

    // Push message from the remote canister, should be in the remote subnet
    // queue.
    state
        .push_input(
            QueueIndex::from(0),
            RequestBuilder::default()
                .sender(remote_canister_id)
                .receiver(local_canister_id)
                .build()
                .into(),
            (u64::MAX / 2).into(),
            &mut (i64::MAX / 2),
        )
        .unwrap();
    assert_eq!(
        state
            .canister_state(&local_canister_id)
            .unwrap()
            .system_state
            .queues()
            .get_remote_subnet_input_schedule()
            .len(),
        1
    );

    // Push message from the local canister, should be in the local subnet queue.
    state
        .push_input(
            QueueIndex::from(0),
            RequestBuilder::default()
                .sender(local_canister_id)
                .receiver(local_canister_id)
                .build()
                .into(),
            (u64::MAX / 2).into(),
            &mut (i64::MAX / 2),
        )
        .unwrap();
    assert_eq!(
        state
            .canister_state(&local_canister_id)
            .unwrap()
            .system_state
            .queues()
            .get_local_subnet_input_schedule()
            .len(),
        1
    );

    // Push message from the local subnet, should be in the local subnet queue.
    state
        .push_input(
            QueueIndex::from(0),
            RequestBuilder::default()
                .sender(CanisterId::new(local_subnet_id.get()).unwrap())
                .receiver(local_canister_id)
                .build()
                .into(),
            (u64::MAX / 2).into(),
            &mut (i64::MAX / 2),
        )
        .unwrap();
    assert_eq!(
        state
            .canister_state(&local_canister_id)
            .unwrap()
            .system_state
            .queues()
            .get_local_subnet_input_schedule()
            .len(),
        2
    );
}

#[test]
fn validate_response_fails_when_unknown_callback_id() {
    let canister_a_id = canister_test_id(0);
    let canister_b_id = canister_test_id(1);
    let mut canister_a = get_running_canister(canister_a_id);

    // Creating response from canister B to canister A.
    let response = RequestOrResponse::Response(
        ResponseBuilder::new()
            .originator(canister_a_id)
            .respondent(canister_b_id)
            .originator_reply_callback(CallbackId::from(1))
            .build()
            .into(),
    );
    assert_eq!(
        canister_a.push_input(
            QueueIndex::new(0),
            response.clone(),
            MAX_CANISTER_MEMORY_SIZE,
            &mut SUBNET_AVAILABLE_MEMORY.clone(),
            SubnetType::Application,
            InputQueueType::RemoteSubnet,
        ),
        Err((
            StateError::NonMatchingResponse {
                err_str: "unknown callback id".to_string(),
                originator: canister_a_id,
                callback_id: CallbackId::from(1),
                respondent: canister_b_id
            },
            response
        ))
    );
}

#[test]
fn validate_responses_against_callback_details() {
    let canister_a_id = canister_test_id(0);
    let canister_b_id = canister_test_id(1);
    let canister_c_id = canister_test_id(2);

    let mut canister_a = get_running_canister(canister_a_id);

    // Creating the CallContext and registering the callback for a request from canister A -> canister B.
    let callback_id_1 = CallbackId::from(1);
    register_callback(&mut canister_a, canister_a_id, canister_b_id, callback_id_1);

    // Creating the CallContext and registering the callback for a request from canister A -> canister C.
    let callback_id_2 = CallbackId::from(2);
    register_callback(&mut canister_a, canister_a_id, canister_c_id, callback_id_2);

    // Reserving slots in the input queue for the corresponding responses.
    // Request from canister A to canister B.
    assert_eq!(
        canister_a.push_output_request(
            RequestBuilder::new()
                .sender(canister_a_id)
                .receiver(canister_b_id)
                .sender_reply_callback(callback_id_1)
                .build()
                .into(),
        ),
        Ok(())
    );
    // Request from canister A to canister C.
    assert_eq!(
        canister_a.push_output_request(
            RequestBuilder::new()
                .sender(canister_a_id)
                .receiver(canister_c_id)
                .sender_reply_callback(callback_id_2)
                .build()
                .into(),
        ),
        Ok(())
    );

    // Creating invalid response from canister C to canister A.
    // Using the callback_id from canister A -> B.
    let response = RequestOrResponse::Response(
        ResponseBuilder::new()
            .originator(canister_a_id)
            .respondent(canister_c_id)
            .originator_reply_callback(callback_id_1)
            .build()
            .into(),
    );
    assert_eq!(
        canister_a.push_input(
            QueueIndex::new(0),
            response.clone(),
            MAX_CANISTER_MEMORY_SIZE,
            &mut SUBNET_AVAILABLE_MEMORY.clone(),
            SubnetType::Application,
            InputQueueType::RemoteSubnet,
        ),
        Err((StateError::NonMatchingResponse { err_str: format!(
            "invalid details, expected => [originator => {}, respondent => {}], but got response with",
            canister_a_id, canister_b_id,
        ), originator: response.receiver(), callback_id: callback_id_1, respondent: response.sender()}, response))
    );

    // Creating valid response from canister C to canister A.
    // Pushing the response in canister A's input queue is successful.
    let response = RequestOrResponse::Response(
        ResponseBuilder::new()
            .originator(canister_a_id)
            .respondent(canister_c_id)
            .originator_reply_callback(callback_id_2)
            .build()
            .into(),
    );
    assert_eq!(
        canister_a.push_input(
            QueueIndex::new(0),
            response,
            MAX_CANISTER_MEMORY_SIZE,
            &mut SUBNET_AVAILABLE_MEMORY.clone(),
            SubnetType::Application,
            InputQueueType::RemoteSubnet,
        ),
        Ok(())
    );

    // Creating valid response from canister B to canister A.
    // Pushing the response in canister A's input queue is successful.
    let response = RequestOrResponse::Response(
        ResponseBuilder::new()
            .originator(canister_a_id)
            .respondent(canister_b_id)
            .originator_reply_callback(callback_id_1)
            .build()
            .into(),
    );
    assert_eq!(
        canister_a.push_input(
            QueueIndex::new(0),
            response,
            MAX_CANISTER_MEMORY_SIZE,
            &mut SUBNET_AVAILABLE_MEMORY.clone(),
            SubnetType::Application,
            InputQueueType::RemoteSubnet,
        ),
        Ok(())
    );
}

#[test]
fn push_request_bitcoin_respects_bitcoin_feature_flag() {
    let mut state =
        ReplicatedState::new_rooted_at(SUBNET_ID, SubnetType::Application, "unused".into());

    let request = BitcoinAdapterRequestWrapper::GetSuccessorsRequest(GetSuccessorsRequest {
        processed_block_hashes: vec![vec![10; 32]],
        anchor: vec![10; 32],
    });

    // Bitcoin feature is disabled, enqueueing a request should fail.
    assert_eq!(
        state.push_request_bitcoin(request.clone()),
        Err(StateError::BitcoinStateError(
            BitcoinStateError::FeatureNotEnabled
        ))
    );

    // Bitcoin feature is paused, enqueueing a request should fail.
    state.metadata.own_subnet_features =
        SubnetFeatures::from_str("bitcoin_testnet_paused").unwrap();
    assert_eq!(
        state.push_request_bitcoin(request.clone()),
        Err(StateError::BitcoinStateError(
            BitcoinStateError::FeatureNotEnabled
        ))
    );

    // Bitcoin feature is enabled, enqueueing a request should succeed.
    state.metadata.own_subnet_features = SubnetFeatures::from_str("bitcoin_testnet").unwrap();
    state.push_request_bitcoin(request).unwrap();
}

#[test]
fn push_response_bitcoin_respects_bitcoin_feature_flag() {
    let mut state =
        ReplicatedState::new_rooted_at(SUBNET_ID, SubnetType::Application, "unused".into());

    let response = BitcoinAdapterResponse {
        response: BitcoinAdapterResponseWrapper::GetSuccessorsResponse(
            GetSuccessorsResponse::default(),
        ),
        callback_id: 0,
    };

    // Bitcoin feature is disabled, enqueueing a response should fail.
    assert_eq!(
        state.push_response_bitcoin(response.clone()),
        Err(StateError::BitcoinStateError(
            BitcoinStateError::FeatureNotEnabled
        ))
    );

    // Enable bitcoin feature and push two requests.
    state.metadata.own_subnet_features = SubnetFeatures::from_str("bitcoin_testnet").unwrap();
    state
        .push_request_bitcoin(BitcoinAdapterRequestWrapper::GetSuccessorsRequest(
            GetSuccessorsRequest {
                processed_block_hashes: vec![vec![10; 32]],
                anchor: vec![10; 32],
            },
        ))
        .unwrap();
    state
        .push_request_bitcoin(BitcoinAdapterRequestWrapper::GetSuccessorsRequest(
            GetSuccessorsRequest {
                processed_block_hashes: vec![vec![20; 32]],
                anchor: vec![20; 32],
            },
        ))
        .unwrap();

    // Pushing a response when bitcoin feature is enabled should succeed.
    state.push_response_bitcoin(response).unwrap();

    // Pause bitcoin feature, responses should still be enqueued successfully.
    state.metadata.own_subnet_features =
        SubnetFeatures::from_str("bitcoin_testnet_paused").unwrap();
    state
        .push_response_bitcoin(BitcoinAdapterResponse {
            response: BitcoinAdapterResponseWrapper::GetSuccessorsResponse(
                GetSuccessorsResponse::default(),
            ),
            callback_id: 1,
        })
        .unwrap();
}

#[test]
fn state_equality_with_bitcoin() {
    let mut state =
        ReplicatedState::new_rooted_at(SUBNET_ID, SubnetType::Application, "unused".into());

    // Enable bitcoin feature.
    state.metadata.own_subnet_features = SubnetFeatures::from_str("bitcoin_testnet").unwrap();

    let original_state = state.clone();

    state
        .push_request_bitcoin(BitcoinAdapterRequestWrapper::GetSuccessorsRequest(
            GetSuccessorsRequest {
                processed_block_hashes: vec![vec![10; 32]],
                anchor: vec![10; 32],
            },
        ))
        .unwrap();

    // The bitcoin state is different and so the states cannot be equal.
    assert_ne!(original_state, state);
}

proptest! {
    #[test]
    fn peek_and_next_consistent(
        (mut replicated_state, _, total_requests) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, Some(8))
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        let mut num_requests = 0;
        while let Some((queue_id, idx, msg)) = output_iter.peek() {
            num_requests += 1;
            assert_eq!(Some((queue_id, idx, msg.clone())), output_iter.next());
        }

        drop(output_iter);
        assert_eq!(total_requests, num_requests);
        assert_eq!(replicated_state.output_message_count(), 0);
    }

    /// Replicated state with multiple canisters, each with multiple output queues
    /// of size 1. Some messages are consumed, some (size 1) queues are excluded.
    ///
    /// Expect consumed + excluded to equal initial size. Expect the messages in
    /// excluded queues to be left in the state.
    #[test]
    fn peek_and_next_consistent_with_ignore(
        (mut replicated_state, _, total_requests) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, None),
        start in 0..=1,
        exclude_step in 2..=5,
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        let mut i = start;
        let mut excluded = 0;
        let mut consumed = 0;
        while let Some((queue_id, idx, msg)) = output_iter.peek() {
            i += 1;
            if i % exclude_step == 0 {
                output_iter.exclude_queue();
                excluded += 1;
            } else {
                assert_eq!(Some((queue_id, idx, msg.clone())), output_iter.next());
                consumed += 1;
            }
        }

        drop(output_iter);
        assert_eq!(total_requests, excluded + consumed);
        assert_eq!(replicated_state.output_message_count(), excluded);
    }

    #[test]
    fn iter_yields_correct_elements(
       (mut replicated_state, mut raw_requests, _total_requests) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, None),
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        for (_, _, msg) in &mut output_iter {
            let mut requests = raw_requests.pop_front().unwrap();
            while requests.is_empty() {
                requests = raw_requests.pop_front().unwrap();
            }

            if let Some(raw_msg) = requests.pop_front() {
                assert_eq!(msg, raw_msg, "Popped message does not correspond with expected message. popped: {:?}. expected: {:?}.", msg, raw_msg);
            } else {
                panic!("Pop yielded an element that was not contained in the respective queue");
            }

            raw_requests.push_back(requests);
        }

        drop(output_iter);
        // Ensure that actually all elements have been consumed.
        assert_eq!(raw_requests.iter().map(|requests| requests.len()).sum::<usize>(), 0);
        assert_eq!(replicated_state.output_message_count(), 0);
    }

    #[test]
    fn iter_with_ignore_yields_correct_elements(
       (mut replicated_state, mut raw_requests, total_requests) in arb_replicated_state_with_queues(SUBNET_ID, 10, 10, None),
        start in 0..=1,
        ignore_step in 2..=5,
    ) {
        let mut consumed = 0;
        let mut ignored_requests = Vec::new();
        // Check whether popping elements with ignores in between yields the expected messages
        {
            let mut output_iter = replicated_state.output_into_iter();

            let mut i = start;
            while let Some((_, _, msg)) = output_iter.peek() {

                let mut requests = raw_requests.pop_front().unwrap();
                while requests.is_empty() {
                    requests = raw_requests.pop_front().unwrap();
                }

                i += 1;
                if i % ignore_step == 0 {
                    // Popping the front of the requests will amount to the same as ignoring as
                    // we use queues of size one in this test.
                    let popped = requests.pop_front().unwrap();
                    assert_eq!(*msg, popped);
                    output_iter.exclude_queue();
                    ignored_requests.push(popped);
                    // We push the queue to the front as the canister gets another chance if one
                    // of its queues are ignored in the current implementation.
                    raw_requests.push_front(requests);
                    continue;
                }

                let (_, _, msg) = output_iter.next().unwrap();
                if let Some(raw_msg) = requests.pop_front() {
                    consumed += 1;
                    assert_eq!(msg, raw_msg, "Popped message does not correspond with expected message. popped: {:?}. expected: {:?}.", msg, raw_msg);
                } else {
                    panic!("Pop yielded an element that was not contained in the respective queue");
                }

                raw_requests.push_back(requests);
            }
        }

        let remaining_output = replicated_state.output_message_count();

        assert_eq!(remaining_output, total_requests - consumed);
        assert_eq!(remaining_output, ignored_requests.len());

        for raw in ignored_requests {
            let queues = if let Some(canister) = replicated_state.canister_states.get_mut(&raw.sender()) {
                canister.system_state.queues_mut()
            } else {
                replicated_state.subnet_queues_mut()
            };

            let (_, msg) = queues.pop_canister_output(&raw.receiver()).unwrap();
            assert_eq!(raw, msg);
        }

        assert_eq!(replicated_state.output_message_count(), 0);

    }

    #[test]
    fn peek_next_loop_terminates(
        (mut replicated_state, _, _) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, Some(8)),
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        while output_iter.peek().is_some() {
            output_iter.next();
        }
    }

    #[test]
    fn ignore_leaves_state_untouched(
        (mut replicated_state, _, _) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, Some(8)),
    ) {
        let expected_state = replicated_state.clone();
        {
            let mut output_iter = replicated_state.output_into_iter();

            while output_iter.peek().is_some() {
                output_iter.exclude_queue();
            }
        }

        assert_eq!(expected_state, replicated_state);
    }

    #[test]
    fn peek_next_loop_with_ignores_terminates(
        (mut replicated_state, _, _) in arb_replicated_state_with_queues(SUBNET_ID, 20, 20, Some(8)),
        start in 0..=1,
        ignore_step in 2..=5,
    ) {
        let mut output_iter = replicated_state.output_into_iter();

        let mut i = start;
        while output_iter.peek().is_some() {
            i += 1;
            if i % ignore_step == 0 {
                output_iter.exclude_queue();
                continue;
            }
            output_iter.next();
        }
    }
}
