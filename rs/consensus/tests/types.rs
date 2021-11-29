use ic_interfaces::crypto::Signable;
use ic_protobuf::types::v1 as pb;
use ic_test_utilities::consensus::{fake::*, make_genesis};
use ic_types::consensus::catchup::*;
use ic_types::consensus::dkg;
use ic_utils::fs::write_protobuf_using_tmp_file;
use std::convert::TryFrom;
use tempfile::Builder;

#[test]
fn ensure_equality_of_signed_bytes_of_catch_up_package_wrappers() {
    let cup = make_genesis(dkg::Summary::fake());
    let protobuf = pb::CatchUpPackage::from(&cup);

    assert_eq!(
        CatchUpContentProtobufBytes(protobuf.content.clone()).as_signed_bytes(),
        cup.content.as_signed_bytes()
    );
    let from_proto = CatchUpPackage::try_from(&protobuf).unwrap();
    assert_eq!(from_proto, cup);

    let filepath = Builder::new().tempfile().unwrap().path().to_path_buf();
    write_protobuf_using_tmp_file(&filepath, &protobuf).unwrap();

    let read_from_file = pb::CatchUpPackage::read_from_file(&filepath)
        .map_err(|e| panic!("Failed to read CUP {}", e))
        .unwrap();

    // Ensure that the value we get after transforming into protobuf, writing to
    // a file, reading from that file back into a protobuf and then into a
    // normal cup is the same as the original value we started with.
    assert_eq!(read_from_file, protobuf);
    assert_eq!(cup, CatchUpPackage::try_from(&read_from_file).unwrap())
}
