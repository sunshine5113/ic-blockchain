use crate::invariants::{
    common::{
        get_all_ecdsa_signing_subnet_list_records, get_node_records_from_snapshot,
        InvariantCheckError, RegistrySnapshot,
    },
    subnet::get_subnet_records_map,
};

use std::collections::{BTreeMap, HashMap, HashSet};

use prost::Message;

use ic_base_types::{subnet_id_try_from_protobuf, NodeId};
use ic_crypto_node_key_validation::ValidNodePublicKeys;
use ic_nns_common::registry::decode_or_panic;
use ic_protobuf::{
    crypto::v1::NodePublicKeys,
    registry::crypto::v1::{PublicKey, X509PublicKeyCert},
};
use ic_registry_keys::{
    get_ecdsa_key_id_from_signing_subnet_list_key, make_node_record_key, make_subnet_record_key,
    maybe_parse_crypto_node_key, maybe_parse_crypto_tls_cert_key, CRYPTO_RECORD_KEY_PREFIX,
    CRYPTO_TLS_CERT_KEY_PREFIX, NODE_RECORD_KEY_PREFIX,
};
use ic_types::crypto::KeyPurpose;

// All crypto public keys found for the nodes or for the subnets in the
// registry.
type AllPublicKeys = BTreeMap<(NodeId, KeyPurpose), PublicKey>;

// All TLS certificates found for the nodes in the registry.
type AllTlsCertificates = BTreeMap<NodeId, X509PublicKeyCert>;

// Checks node invariants related to crypto keys:
//  * every node has the required public keys and these keys are well formed and
//    valid. The required keys are:
//     - node signing public key
//     - committee signing public key
//     - DKG dealing encryption public key
//     - TLS certificate
//     - interactive DKG encryption public key
//  * All public keys and TLS certificates have a corresponding node
//  * every node's id (node_id) is correctly derived from its node signing
//    public key
//  * all the public keys and all the TLS certificates belonging to the all the
//    nodes are unique
//  * At most 1 subnet can be an ECDSA signing subnet for a given key_id (for now)
//  * Subnets specified in ECDSA signing subnet lists exists and contain the equivalent key in their configs
//
// TODO(NNS1-202): should we also check that there are no "left-over" public
// keys or TLS certificates in the registry, i.e. every key/certificate is
// assigned to some existing node?
pub(crate) fn check_node_crypto_keys_invariants(
    snapshot: &RegistrySnapshot,
) -> Result<(), InvariantCheckError> {
    // TODO(NNS1-202): re-enable these invariants
    if false {
        let nodes = get_node_records_from_snapshot(snapshot);
        let mut pks = get_all_nodes_public_keys(snapshot);
        let mut certs = get_all_tls_certs(snapshot);
        let mut unique_pks: BTreeMap<Vec<u8>, NodeId> = BTreeMap::new();
        let mut unique_certs: HashMap<Vec<u8>, NodeId> = HashMap::new();

        for node_id in nodes.keys() {
            let valid_node_pks = check_node_keys(node_id, &mut pks, &mut certs)?;
            check_node_keys_are_unique(&valid_node_pks, &mut unique_pks)?;
            check_tls_certs_are_unique(&valid_node_pks, &mut unique_certs)?;
        }
    }

    check_no_orphaned_node_crypto_records(snapshot)?;

    check_ecdsa_signing_subnet_lists(snapshot)
}

// Returns all nodes' public keys in the snapshot.
fn get_all_nodes_public_keys(snapshot: &RegistrySnapshot) -> AllPublicKeys {
    let mut pks = BTreeMap::new();
    for (k, v) in snapshot {
        if k.starts_with(CRYPTO_RECORD_KEY_PREFIX.as_bytes()) {
            let (node_id, key_purpose) = maybe_parse_crypto_node_key(
                &String::from_utf8(k.to_owned()).expect("invalid crypto node key bytes"),
            )
            .expect("invalid crypto node key");
            let pk = decode_or_panic::<PublicKey>(v.clone());
            pks.insert((node_id, key_purpose), pk);
        }
    }
    pks
}

// Returns all TLS certificates in the snapshot.
fn get_all_tls_certs(snapshot: &RegistrySnapshot) -> AllTlsCertificates {
    let mut certs = BTreeMap::new();
    for (k, v) in snapshot {
        if k.starts_with(CRYPTO_TLS_CERT_KEY_PREFIX.as_bytes()) {
            let node_id = maybe_parse_crypto_tls_cert_key(
                &String::from_utf8(k.to_owned()).expect("invalid tls cert key bytes"),
            )
            .expect("invalid tls cert key");
            let cert = decode_or_panic::<X509PublicKeyCert>(v.clone());
            certs.insert(node_id, cert);
        }
    }
    certs
}

fn check_node_keys(
    node_id: &NodeId,
    pks: &mut AllPublicKeys,
    certs: &mut AllTlsCertificates,
) -> Result<ValidNodePublicKeys, InvariantCheckError> {
    let idkg_dealing_encryption_pk = pks.remove(&(*node_id, KeyPurpose::IDkgMEGaEncryption));
    let npk = NodePublicKeys {
        version: match idkg_dealing_encryption_pk {
            Some(_) => 1,
            None => 0,
        },
        node_signing_pk: pks.remove(&(*node_id, KeyPurpose::NodeSigning)),
        committee_signing_pk: pks.remove(&(*node_id, KeyPurpose::CommitteeSigning)),
        dkg_dealing_encryption_pk: pks.remove(&(*node_id, KeyPurpose::DkgDealingEncryption)),
        tls_certificate: certs.remove(node_id),
        idkg_dealing_encryption_pk,
    };
    let vnpk = ValidNodePublicKeys::try_from(npk, *node_id).map_err(|e| InvariantCheckError {
        msg: format!(
            "crypto key validation for node {} failed with {}",
            node_id, e
        ),
        source: None,
    })?;
    Ok(vnpk)
}

fn check_node_keys_are_unique(
    node_pks: &ValidNodePublicKeys,
    unique_pks: &mut BTreeMap<Vec<u8>, NodeId>,
) -> Result<(), InvariantCheckError> {
    let mut pubkeys = vec![
        node_pks.node_signing_key(),
        node_pks.committee_signing_key(),
        node_pks.dkg_dealing_encryption_key(),
    ];
    if let Some(idkg) = node_pks.idkg_dealing_encryption_key() {
        pubkeys.push(idkg);
    }
    for pk in pubkeys {
        let mut pk_bytes: Vec<u8> = vec![];
        pk.encode(&mut pk_bytes).expect("encode cannot fail.");
        match unique_pks.get(&pk_bytes) {
            Some(existing_id) => {
                return Err(InvariantCheckError {
                    msg: format!(
                        "nodes {} and {} use the same public key {:?}",
                        existing_id,
                        node_pks.node_id(),
                        pk
                    ),
                    source: None,
                })
            }
            None => {
                unique_pks.insert(pk_bytes, node_pks.node_id());
            }
        }
    }
    Ok(())
}

fn check_tls_certs_are_unique(
    node_pks: &ValidNodePublicKeys,
    unique_certs: &mut HashMap<Vec<u8>, NodeId>,
) -> Result<(), InvariantCheckError> {
    let mut cert_bytes: Vec<u8> = vec![];
    node_pks
        .tls_certificate()
        .encode(&mut cert_bytes)
        .expect("encode cannot fail.");
    match unique_certs.get(&cert_bytes) {
        Some(existing_id) => Err(InvariantCheckError {
            msg: format!(
                "nodes {} and {} use the same TLS certificate {:?}",
                existing_id,
                node_pks.node_id(),
                node_pks.tls_certificate()
            ),
            source: None,
        }),
        None => {
            unique_certs.insert(cert_bytes, node_pks.node_id());
            Ok(())
        }
    }
}

fn check_ecdsa_signing_subnet_lists(
    snapshot: &RegistrySnapshot,
) -> Result<(), InvariantCheckError> {
    let subnet_records_map = get_subnet_records_map(snapshot);

    get_all_ecdsa_signing_subnet_list_records(snapshot)
        .iter()
        .try_for_each(|(key_id, ecdsa_signing_subnet_list)| {
            if ecdsa_signing_subnet_list.subnets.len() > 1 {
                return Err(InvariantCheckError {
                    msg: format!(
                        "key_id {} ended up with more than one ECDSA signing subnet",
                        key_id
                    ),
                    source: None,
                });
            }

            let ecdsa_key_id =  match get_ecdsa_key_id_from_signing_subnet_list_key(key_id) {
                Ok(ecdsa_key_id) => ecdsa_key_id,
                Err(error) => {
                    return Err(InvariantCheckError {
                        msg: format!(
                            "Registry key_id {} could not be converted to an ECDSA signature key id: {:?}",
                            key_id,
                            error,
                        ),
                        source: None,
                    });
                }
            };

            ecdsa_signing_subnet_list
                .subnets
                .iter()
                .try_for_each(|subnet_id_bytes| {
                    let subnet_id = subnet_id_try_from_protobuf(subnet_id_bytes.clone()).unwrap();

                    subnet_records_map
                        .get(&make_subnet_record_key(subnet_id).into_bytes())
                        .ok_or(InvariantCheckError {
                            msg: format!(
                                "A non-existent subnet {} was set as the holder of a key_id {}",
                                subnet_id, key_id
                            ),
                            source: None,
                        })?
                        .ecdsa_config
                        .as_ref()
                        .ok_or(InvariantCheckError {
                            msg: format!("The subnet {} does not have an ECDSA config", subnet_id),
                            source: None,
                        })?
                        .key_ids
                        .contains(&(&ecdsa_key_id).into())
                        .then(|| ())
                        .ok_or(InvariantCheckError {
                            msg: format!(
                                "The subnet {} does not have the key with {} in its ecdsa configurations",
                                subnet_id, key_id
                            ),
                            source: None,
                        })
                })
        })
}

fn check_no_orphaned_node_crypto_records(
    snapshot: &RegistrySnapshot,
) -> Result<(), InvariantCheckError> {
    // Collect unique node_ids from crypto and tls records
    let mut nodes_with_records: HashSet<NodeId> = HashSet::new();
    for key in snapshot.keys() {
        let key_string = String::from_utf8(key.clone()).unwrap();
        if let Some((node_id, _)) = maybe_parse_crypto_node_key(&key_string) {
            nodes_with_records.insert(node_id);
        } else if let Some(node_id) = maybe_parse_crypto_tls_cert_key(&key_string) {
            nodes_with_records.insert(node_id);
        }
    }

    // Filter to only node_ids that do not have a node_record in the registry
    let nodes_with_orphaned_records = nodes_with_records
        .into_iter()
        .filter(|node_id| {
            snapshot
                .get(make_node_record_key(*node_id).as_bytes())
                .is_none()
        })
        .collect::<Vec<_>>();

    // There should be no crypto or tls records without a node_record
    if !nodes_with_orphaned_records.is_empty() {
        return Err(InvariantCheckError {
            msg: format!(
                "There are {} or {} entries without a corresponding {} entry: {:?}",
                CRYPTO_RECORD_KEY_PREFIX,
                CRYPTO_TLS_CERT_KEY_PREFIX,
                NODE_RECORD_KEY_PREFIX,
                nodes_with_orphaned_records
            ),
            source: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_crypto::utils::get_node_keys_or_generate_if_missing;
    use ic_nns_common::registry::encode_or_panic;
    use ic_protobuf::registry::crypto::v1::AlgorithmId as AlgorithmIdProto;
    use ic_protobuf::registry::node::v1::NodeRecord;
    use ic_registry_keys::{make_crypto_node_key, make_crypto_tls_cert_key, make_node_record_key};
    use ic_test_utilities::crypto::temp_dir::temp_dir;

    fn insert_node_crypto_keys(
        node_id: &NodeId,
        node_pks: &NodePublicKeys,
        snapshot: &mut RegistrySnapshot,
    ) {
        if node_pks.node_signing_pk.is_some() {
            snapshot.insert(
                make_crypto_node_key(node_id.to_owned(), KeyPurpose::NodeSigning).into_bytes(),
                encode_or_panic::<PublicKey>(&node_pks.node_signing_pk.clone().unwrap()),
            );
        };
        if node_pks.committee_signing_pk.is_some() {
            snapshot.insert(
                make_crypto_node_key(node_id.to_owned(), KeyPurpose::CommitteeSigning).into_bytes(),
                encode_or_panic::<PublicKey>(&node_pks.committee_signing_pk.clone().unwrap()),
            );
        };
        if node_pks.dkg_dealing_encryption_pk.is_some() {
            snapshot.insert(
                make_crypto_node_key(node_id.to_owned(), KeyPurpose::DkgDealingEncryption)
                    .into_bytes(),
                encode_or_panic::<PublicKey>(&node_pks.dkg_dealing_encryption_pk.clone().unwrap()),
            );
        };
        if node_pks.idkg_dealing_encryption_pk.is_some() {
            snapshot.insert(
                make_crypto_node_key(node_id.to_owned(), KeyPurpose::IDkgMEGaEncryption)
                    .into_bytes(),
                encode_or_panic::<PublicKey>(&node_pks.idkg_dealing_encryption_pk.clone().unwrap()),
            );
        };
        if node_pks.tls_certificate.is_some() {
            snapshot.insert(
                make_crypto_tls_cert_key(node_id.to_owned()).into_bytes(),
                encode_or_panic::<X509PublicKeyCert>(&node_pks.tls_certificate.clone().unwrap()),
            );
        };
    }

    fn valid_node_keys_and_node_id() -> (NodePublicKeys, NodeId) {
        let temp_dir = temp_dir();
        get_node_keys_or_generate_if_missing(temp_dir.path())
    }

    fn insert_dummy_node(node_id: &NodeId, snapshot: &mut RegistrySnapshot) {
        snapshot.insert(
            make_node_record_key(node_id.to_owned()).into_bytes(),
            encode_or_panic::<NodeRecord>(&NodeRecord::default()),
        );
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_valid_snapshot() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);
        insert_node_crypto_keys(&node_id_2, &node_pks_2, &mut snapshot);
        assert!(check_node_crypto_keys_invariants(&snapshot).is_ok());
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_missing_committee_key() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let incomplete_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_2.node_signing_pk,
            committee_signing_pk: None,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: node_pks_2.idkg_dealing_encryption_pk,
            tls_certificate: node_pks_2.tls_certificate,
        };
        insert_node_crypto_keys(&node_id_2, &incomplete_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err.to_string().contains("committee"));
        assert!(err.to_string().contains("key is missing"));
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_missing_node_signing_key() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let incomplete_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: None,
            committee_signing_pk: node_pks_2.committee_signing_pk,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: node_pks_2.idkg_dealing_encryption_pk,
            tls_certificate: node_pks_2.tls_certificate,
        };
        insert_node_crypto_keys(&node_id_2, &incomplete_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err.to_string().contains("node signing key"));
        assert!(err.to_string().contains("key is missing"));
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_missing_idkg_dealing_encryption_key() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let incomplete_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_2.node_signing_pk,
            committee_signing_pk: node_pks_2.committee_signing_pk,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: None,
            tls_certificate: node_pks_2.tls_certificate,
        };
        insert_node_crypto_keys(&node_id_2, &incomplete_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);

        // Currently a missing I-DKG dealing encryption key shall not lead to a
        // validation error because we are in a transition phase where not all nodes
        // have such a key yet. Once all nodes have this key, this test needs to be
        // adapted (see CRP-1422).
        assert!(result.is_ok());
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_missing_tls_cert() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let incomplete_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_2.node_signing_pk,
            committee_signing_pk: node_pks_2.committee_signing_pk,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: node_pks_2.idkg_dealing_encryption_pk,
            tls_certificate: None,
        };
        insert_node_crypto_keys(&node_id_2, &incomplete_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err.to_string().contains("certificate"));
        assert!(err.to_string().contains("missing"));
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_invalid_dkg_encryption_key() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let invalid_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_2.node_signing_pk,
            committee_signing_pk: node_pks_2.committee_signing_pk,
            dkg_dealing_encryption_pk: Some(PublicKey {
                version: 0,
                algorithm: 0,
                key_value: vec![],
                proof_data: None,
            }),
            idkg_dealing_encryption_pk: node_pks_2.idkg_dealing_encryption_pk,
            tls_certificate: node_pks_2.tls_certificate,
        };
        insert_node_crypto_keys(&node_id_2, &invalid_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err
            .to_string()
            .contains("invalid DKG dealing encryption key"));
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_invalid_idkg_encryption_key() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let invalid_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_2.node_signing_pk,
            committee_signing_pk: node_pks_2.committee_signing_pk,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: Some(PublicKey {
                version: 0,
                algorithm: AlgorithmIdProto::MegaSecp256k1 as i32,
                key_value: vec![],
                proof_data: None,
            }),
            tls_certificate: node_pks_2.tls_certificate,
        };
        insert_node_crypto_keys(&node_id_2, &invalid_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err
            .to_string()
            .contains("invalid I-DKG dealing encryption key: verification failed"));
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_duplicated_committee_key() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let duplicated_key_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_2.node_signing_pk,
            committee_signing_pk: node_pks_1.committee_signing_pk,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: node_pks_2.idkg_dealing_encryption_pk,
            tls_certificate: node_pks_2.tls_certificate,
        };
        insert_node_crypto_keys(&node_id_2, &duplicated_key_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_1.to_string()));
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err.to_string().contains("the same public key"));
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_duplicated_idkg_encryption_key() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let duplicated_key_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_2.node_signing_pk,
            committee_signing_pk: node_pks_2.committee_signing_pk,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: node_pks_1.idkg_dealing_encryption_pk,
            tls_certificate: node_pks_2.tls_certificate,
        };
        insert_node_crypto_keys(&node_id_2, &duplicated_key_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_1.to_string()));
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err.to_string().contains("the same public key"));
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_duplicated_tls_cert() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let duplicated_cert_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_2.node_signing_pk,
            committee_signing_pk: node_pks_2.committee_signing_pk,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: node_pks_2.idkg_dealing_encryption_pk,
            tls_certificate: node_pks_1.tls_certificate,
        };
        insert_node_crypto_keys(&node_id_2, &duplicated_cert_node_pks, &mut snapshot);
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err.to_string().contains("invalid TLS certificate"));
    }

    // TODO(NNS1-202): re-enable these tests
    #[ignore]
    #[test]
    fn node_crypto_keys_invariants_inconsistent_node_id() {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();
        let (node_pks_2, node_id_2) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_dummy_node(&node_id_2, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);

        let (node_pks_3, _node_id_3) = valid_node_keys_and_node_id();
        let inconsistent_signing_key_node_pks = NodePublicKeys {
            version: node_pks_2.version,
            node_signing_pk: node_pks_3.node_signing_pk,
            committee_signing_pk: node_pks_2.committee_signing_pk,
            dkg_dealing_encryption_pk: node_pks_2.dkg_dealing_encryption_pk,
            idkg_dealing_encryption_pk: node_pks_2.idkg_dealing_encryption_pk,
            tls_certificate: node_pks_2.tls_certificate,
        };
        insert_node_crypto_keys(
            &node_id_2,
            &inconsistent_signing_key_node_pks,
            &mut snapshot,
        );
        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&node_id_2.to_string()));
        assert!(err.to_string().contains("invalid node signing key"));
    }

    #[test]
    fn orphaned_crypto_node_signing_pk() {
        let (mut orphaned_keys, missing_node) = valid_node_keys_and_node_id();
        // This leaves only node_signing_pk as orphan.
        orphaned_keys.committee_signing_pk = None;
        orphaned_keys.tls_certificate = None;
        orphaned_keys.dkg_dealing_encryption_pk = None;
        orphaned_keys.idkg_dealing_encryption_pk = None;
        run_test_orphaned_crypto_keys(missing_node, orphaned_keys);
    }

    #[test]
    fn orphaned_crypto_committee_signing_pk() {
        let (mut orphaned_keys, missing_node) = valid_node_keys_and_node_id();
        orphaned_keys.node_signing_pk = None;
        // This leaves only committee_signing_pk as orphan.
        orphaned_keys.tls_certificate = None;
        orphaned_keys.dkg_dealing_encryption_pk = None;
        orphaned_keys.idkg_dealing_encryption_pk = None;
        run_test_orphaned_crypto_keys(missing_node, orphaned_keys);
    }

    #[test]
    fn orphaned_crypto_tls_certificate() {
        let (mut orphaned_keys, missing_node) = valid_node_keys_and_node_id();
        orphaned_keys.node_signing_pk = None;
        orphaned_keys.committee_signing_pk = None;
        // This leaves only tls_certificate as orphan.
        orphaned_keys.dkg_dealing_encryption_pk = None;
        orphaned_keys.idkg_dealing_encryption_pk = None;
        run_test_orphaned_crypto_keys(missing_node, orphaned_keys);
    }

    #[test]
    fn orphaned_crypto_dkg_dealing_encryption_pk() {
        let (mut orphaned_keys, missing_node) = valid_node_keys_and_node_id();
        orphaned_keys.node_signing_pk = None;
        orphaned_keys.committee_signing_pk = None;
        orphaned_keys.tls_certificate = None;
        // This leaves only dkg_dealing_encryption_pk as orphan.
        orphaned_keys.idkg_dealing_encryption_pk = None;
        run_test_orphaned_crypto_keys(missing_node, orphaned_keys);
    }

    #[test]
    fn orphaned_crypto_idkg_dealing_encryption_pk() {
        let (mut orphaned_keys, missing_node) = valid_node_keys_and_node_id();
        orphaned_keys.node_signing_pk = None;
        orphaned_keys.committee_signing_pk = None;
        orphaned_keys.tls_certificate = None;
        orphaned_keys.dkg_dealing_encryption_pk = None;
        // This leaves only idkg_dealing_encryption_pk as orphan.
        run_test_orphaned_crypto_keys(missing_node, orphaned_keys);
    }

    /// Ensures that if there are any missing keys, the InvariantCheck is triggered for the 'missing_node_id', which
    /// is not given an entry in the nodes table but will have the public_key records created for it
    /// This is useful so that we can run the same test on each individual missing key
    fn run_test_orphaned_crypto_keys(
        missing_node_id: NodeId,
        node_pks_with_missing_entries: NodePublicKeys,
    ) {
        // Crypto keys for the test.
        let (node_pks_1, node_id_1) = valid_node_keys_and_node_id();

        // Generate and check a valid snapshot.
        let mut snapshot = RegistrySnapshot::new();
        insert_dummy_node(&node_id_1, &mut snapshot);
        insert_node_crypto_keys(&node_id_1, &node_pks_1, &mut snapshot);
        insert_node_crypto_keys(
            &missing_node_id,
            &node_pks_with_missing_entries,
            &mut snapshot,
        );

        // TODO make this test more robust (all the cases 1 at a time)

        let result = check_node_crypto_keys_invariants(&snapshot);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains(&missing_node_id.to_string()));
        assert_eq!(
            err.to_string(),
            format!(
                "InvariantCheckError: There are {} or {} entries without a corresponding {} entry: [{}]",
                CRYPTO_RECORD_KEY_PREFIX, CRYPTO_TLS_CERT_KEY_PREFIX, NODE_RECORD_KEY_PREFIX, missing_node_id
            )
        );
    }
}
