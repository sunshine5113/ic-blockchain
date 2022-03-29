use super::test_fixtures::*;
use crate::{encoding::*, max_supported_certification_version};

#[test]
fn roundtrip_encoding_stream_header() {
    for certification_version in 0..=max_supported_certification_version() {
        let header = stream_header(certification_version);

        assert_eq!(
            header,
            decode_stream_header(&encode_stream_header(&header, certification_version)).unwrap()
        );
    }
}

#[test]
fn roundtrip_encoding_request() {
    let request = request();

    for certification_version in 0..=max_supported_certification_version() {
        assert_eq!(
            request,
            decode_message(&encode_message(&request, certification_version)).unwrap()
        );
    }
}

#[test]
fn roundtrip_encoding_response() {
    let response = response();

    for certification_version in 0..=max_supported_certification_version() {
        assert_eq!(
            response,
            decode_message(&encode_message(&response, certification_version)).unwrap()
        );
    }
}

#[test]
fn roundtrip_encoding_reject_response() {
    let reject = reject_response();

    for certification_version in 0..=max_supported_certification_version() {
        assert_eq!(
            reject,
            decode_message(&encode_message(&reject, certification_version)).unwrap()
        );
    }
}
