use criterion::{black_box, BatchSize, BenchmarkId, Criterion};
use criterion_time::ProcessTime;
use ic_base_types::NumBytes;
use ic_canonical_state::{
    hash_tree::{crypto_hash_lazy_tree, hash_lazy_tree},
    lazy_tree::LazyTree,
};
use ic_crypto_tree_hash::{
    flatmap, FlatMap, Label, LabeledTree, MixedHashTree, WitnessGenerator, WitnessGeneratorImpl,
};
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::{
    metadata_state::Stream, testing::ReplicatedStateTesting, ReplicatedState,
};
use ic_state_manager::{stream_encoding::encode_stream_slice, tree_hash::hash_state};
use ic_test_utilities::{
    mock_time,
    types::{
        ids::{canister_test_id, message_test_id, subnet_test_id, user_test_id},
        messages::{RequestBuilder, ResponseBuilder},
    },
};
use ic_types::{
    messages::{CallbackId, Payload},
    xnet::StreamIndex,
    Cycles,
};
use std::convert::TryFrom;

fn bench_traversal(c: &mut Criterion<ProcessTime>) {
    const NUM_STREAM_MESSAGES: u64 = 1_000;
    const NUM_STATUSES: u64 = 30_000;

    let subnet_type = SubnetType::Application;
    let mut state = ReplicatedState::new_rooted_at(subnet_test_id(1), subnet_type, "TEST".into());

    state.modify_streams(|streams| {
        for remote_subnet in 2..10 {
            let mut stream = Stream::default();

            for i in 0..NUM_STREAM_MESSAGES {
                stream.increment_signals_end();
                let msg = if i % 2 == 0 {
                    RequestBuilder::new()
                        .receiver(canister_test_id(i))
                        .sender(canister_test_id(i))
                        .sender_reply_callback(CallbackId::from(i))
                        .payment(Cycles::from(10))
                        .method_name("test".to_string())
                        .method_payload(vec![1; 100])
                        .build()
                        .into()
                } else {
                    ResponseBuilder::new()
                        .originator(canister_test_id(i))
                        .respondent(canister_test_id(i))
                        .originator_reply_callback(CallbackId::from(i))
                        .refund(Cycles::from(10))
                        .response_payload(Payload::Data(vec![2, 100]))
                        .build()
                        .into()
                };
                stream.push(msg);
            }

            streams.insert(subnet_test_id(remote_subnet), stream);
        }
    });

    let user_id = user_test_id(1);
    let time = mock_time();

    for i in 1..NUM_STATUSES {
        use ic_error_types::{ErrorCode, UserError};
        use ic_types::ingress::{IngressState::*, IngressStatus::*, WasmResult::*};

        let status = match i % 6 {
            0 => Known {
                receiver: canister_test_id(i).get(),
                user_id,
                time,
                state: Received,
            },
            1 => Known {
                receiver: canister_test_id(i).get(),
                user_id,
                time,
                state: Completed(Reply(vec![1; 100])),
            },
            2 => Known {
                receiver: canister_test_id(i).get(),
                user_id,
                time,
                state: Completed(Reject("bad request".to_string())),
            },
            3 => Known {
                receiver: canister_test_id(i).get(),
                user_id,
                time,
                state: Failed(UserError::new(
                    ErrorCode::CanisterNotFound,
                    "canister XXX not found",
                )),
            },
            4 => Known {
                receiver: canister_test_id(i).get(),
                user_id,
                time,
                state: Processing,
            },
            5 => Unknown,
            _ => unreachable!(),
        };
        state.set_ingress_status(message_test_id(i), status, NumBytes::from(u64::MAX));
    }

    assert_eq!(
        hash_state(&state).digest(),
        hash_lazy_tree(&LazyTree::from(&state)).root_hash(),
    );

    c.bench_function("traverse/hash_tree", |b| {
        b.iter(|| black_box(hash_state(&state)));
    });

    c.bench_function("traverse/hash_tree_new", |b| {
        b.iter(|| black_box(hash_lazy_tree(&LazyTree::from(&state))))
    });

    c.bench_function("traverse/hash_tree_direct", |b| {
        b.iter(|| black_box(crypto_hash_lazy_tree(&LazyTree::from(&state))))
    });

    c.bench_function("traverse/encode_streams", |b| {
        b.iter(|| {
            black_box(encode_stream_slice(
                &state,
                subnet_test_id(2),
                StreamIndex::from(0),
                StreamIndex::from(100),
                None,
            ))
        });
    });

    c.bench_function("traverse/build_witness_gen", |b| {
        let hash_tree = hash_state(&state);
        b.iter(|| {
            black_box(WitnessGeneratorImpl::try_from(hash_tree.clone()).unwrap());
        })
    });

    c.bench_function("traverse/certify_response/1", |b| {
        use LabeledTree::*;
        let hash_tree = hash_state(&state);
        let witness_gen = WitnessGeneratorImpl::try_from(hash_tree).unwrap();

        let data_tree = SubTree(flatmap! {
            Label::from("request_status") => SubTree(flatmap!{
                Label::from(message_test_id(13)) => SubTree(flatmap!{
                    Label::from("reply") => Leaf(vec![1; 100]),
                    Label::from("status") => Leaf(b"replied".to_vec()),
                })
            })
        });

        b.iter(|| {
            black_box(witness_gen.mixed_hash_tree(&data_tree).unwrap());
        });
    });

    let data_tree_100_statuses = {
        use LabeledTree::*;

        let replied_tree = SubTree(flatmap! {
            Label::from("reply") => Leaf(vec![1; 100]),
            Label::from("status") => Leaf(b"replied".to_vec()),
        });

        let entries: Vec<_> = (1..100)
            .map(|i| {
                (
                    Label::from(message_test_id(1 + 6 * i)),
                    replied_tree.clone(),
                )
            })
            .collect();

        SubTree(flatmap! {
            Label::from("request_status") => SubTree(FlatMap::from_key_values(entries))
        })
    };

    c.bench_function("traverse/certify_response/100", |b| {
        let hash_tree = hash_state(&state);
        let witness_gen = WitnessGeneratorImpl::try_from(hash_tree).unwrap();

        b.iter(|| {
            black_box(
                witness_gen
                    .mixed_hash_tree(&data_tree_100_statuses)
                    .unwrap(),
            );
        });
    });

    c.bench_function("traverse/certify_response/100/new", |b| {
        let hash_tree = hash_lazy_tree(&LazyTree::from(&state));
        b.iter(|| {
            black_box(hash_tree.witness::<MixedHashTree>(&data_tree_100_statuses));
        });
    });

    let mut group = c.benchmark_group("drop_tree");
    group.bench_function(BenchmarkId::new("crypto::HashTree", NUM_STATUSES), |b| {
        let hash_tree = hash_state(&state);
        b.iter_batched(|| hash_tree.clone(), std::mem::drop, BatchSize::LargeInput)
    });
    group.bench_function(
        BenchmarkId::new("canonical_state::HashTree", NUM_STATUSES),
        |b| {
            let hash_tree = hash_lazy_tree(&LazyTree::from(&state));
            b.iter_batched(|| hash_tree.clone(), std::mem::drop, BatchSize::LargeInput)
        },
    );
    group.finish();
}

fn main() {
    let mut c = Criterion::default()
        .with_measurement(ProcessTime::UserTime)
        .sample_size(20)
        .configure_from_args();
    bench_traversal(&mut c);
    c.final_summary();
}
