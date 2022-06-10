use crate::ids::{canister_test_id, node_test_id, subnet_test_id, user_test_id};
use ic_types::{
    crypto::{AlgorithmId, KeyId, KeyPurpose, UserPublicKey},
    messages::{CallbackId, Payload, RejectContext, Request, RequestOrResponse, Response},
    time::UNIX_EPOCH,
    xnet::StreamIndex,
    CanisterId, Cycles, Height, IDkgId, NodeId, RegistryVersion, SubnetId, Time, UserId,
};
use proptest::prelude::*;
use std::{convert::TryInto, time::Duration};
use strum::IntoEnumIterator;

prop_compose! {
    /// Returns an arbitrary [`NodeId`].
    pub fn node_id()(id in any::<u64>()) -> NodeId {
        node_test_id(id)
    }
}

prop_compose! {
    /// Returns an arbitrary [`UserId`].
    pub fn user_id()(id in any::<u64>()) -> UserId {
        user_test_id(id)
    }
}

prop_compose! {
    /// Returns an arbitrary [`CanisterId`].
    pub fn canister_id()(id in any::<u64>()) -> CanisterId {
        canister_test_id(id)
    }
}

prop_compose! {
    /// Returns an arbitrary [`KeyId`].
    pub fn key_id()(id in any::<[u8;32]>()) -> KeyId {
        KeyId::from(id)
    }
}

prop_compose! {
    /// Returns an arbitrary [`KeyPurpose`].
    pub fn key_purpose() (seed in any::<usize>()) -> KeyPurpose {
        let options: Vec<KeyPurpose> = KeyPurpose::iter().collect();
        options[seed % options.len()]
    }
}

prop_compose! {
    /// Returns an arbitrary [`AlgorithmId`].
    pub fn algorithm_id() (seed in any::<usize>()) -> AlgorithmId {
        let options: Vec<AlgorithmId> = AlgorithmId::iter().collect();
        options[seed % options.len()]
    }
}

prop_compose! {
    /// Returns an arbitrary [`RegistryVersion`].
    pub fn registry_version() (seed in any::<u64>()) -> RegistryVersion {
        RegistryVersion::from(seed)
    }
}

prop_compose! {
    /// Returns an arbitrary [`Time`].
    pub fn time() (seed in any::<u64>()) -> Time {
        UNIX_EPOCH + Duration::from_millis(seed)
    }
}

prop_compose! {
    /// Returns an arbitrary [`UserPublicKey`].
    pub fn user_public_key() (
      key in any::<Vec<u8>>(),
      algorithm_id in algorithm_id()
    ) -> UserPublicKey {
        UserPublicKey {
            key,
            algorithm_id,
        }
    }
}

prop_compose! {
    /// Returns an arbitrary [`Height`].
    pub fn height() (
      height in any::<u64>(),
    ) -> Height {
        Height::from(height)
    }
}

prop_compose! {
    /// Returns an arbitrary [`SubnetId`].
    pub fn subnet_id() (
      subnet_id in any::<u64>(),
    ) -> SubnetId {
        subnet_test_id(subnet_id)
    }
}

prop_compose! {
    /// Returns an arbitrary [`IDkgId`].
    pub fn dkg_id() (
      instance_id in height(),
      subnet_id in subnet_id()
    ) -> IDkgId {
        IDkgId {
            instance_id,
            subnet_id,
        }
    }
}

prop_compose! {
    /// Returns an arbitrary [`Request`].
    pub fn request()(
        receiver in canister_id(),
        sender in canister_id(),
        cycles_payment in any::<u64>(),
        method_name in "[a-zA-Z]{1,6}",
        callback in any::<u64>(),
        method_payload in prop::collection::vec(any::<u8>(), 0..16),
    ) -> Request {
        Request {
            receiver,
            sender,
            sender_reply_callback: CallbackId::from(callback),
            payment: Cycles::from(cycles_payment),
            method_name,
            method_payload,
        }
    }
}

/// Produces an arbitrary response [`Payload`].
pub fn response_payload() -> impl Strategy<Value = Payload> {
    prop_oneof![
        // Data payload.
        prop::collection::vec(any::<u8>(), 0..16).prop_flat_map(|data| Just(Payload::Data(data))),
        // Reject payload.
        (1u64..5, "[a-zA-Z]{1,6}").prop_flat_map(|(code, message)| Just(Payload::Reject(
            RejectContext {
                code: code.try_into().unwrap(),
                message
            }
        )))
    ]
}

prop_compose! {
    /// Returns an arbitrary [`Response`].
    pub fn response()(
        originator in canister_id(),
        respondent in canister_id(),
        callback in any::<u64>(),
        cycles_refund in any::<u64>(),
        response_payload in response_payload(),
    ) -> Response {
        Response {
            originator,
            respondent,
            originator_reply_callback: CallbackId::from(callback),
            refund: Cycles::from(cycles_refund),
            response_payload
        }
    }
}

/// Produces an arbitrary [`RequestOrResponse`].
pub fn request_or_response() -> impl Strategy<Value = RequestOrResponse> {
    prop_oneof![
        request().prop_flat_map(|req| Just(req.into())),
        response().prop_flat_map(|rep| Just(rep.into())),
    ]
}

prop_compose! {
    /// Returns an arbitrary [`StreamIndex`] in the `[0, max)` range.
    pub fn stream_index(max: u64) (
      index in 0..max,
    ) -> StreamIndex {
        StreamIndex::from(index)
    }
}
