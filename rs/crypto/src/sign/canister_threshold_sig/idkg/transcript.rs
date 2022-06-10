//! Implementations of IDkgProtocol related to transcripts
use crate::sign::canister_threshold_sig::idkg::complaint::verify_complaint;
use crate::sign::canister_threshold_sig::idkg::utils::{
    get_mega_pubkey, index_and_dealing_of_dealer,
};
use crate::sign::multi_sig::MultiSigVerifierInternal;
use ic_crypto_internal_csp::api::CspIDkgProtocol;
use ic_crypto_internal_csp::api::CspSigner;
use ic_crypto_internal_threshold_sig_ecdsa::{
    CommitmentOpening, IDkgComplaintInternal, IDkgDealingInternal, IDkgTranscriptInternal,
    IDkgTranscriptOperationInternal,
};
use ic_interfaces::registry::RegistryClient;
use ic_types::crypto::canister_threshold_sig::error::{
    IDkgCreateTranscriptError, IDkgLoadTranscriptError, IDkgOpenTranscriptError,
    IDkgVerifyOpeningError, IDkgVerifyTranscriptError,
};
use ic_types::crypto::canister_threshold_sig::idkg::{
    IDkgComplaint, IDkgMultiSignedDealing, IDkgOpening, IDkgTranscript, IDkgTranscriptParams,
    IDkgTranscriptType,
};
use ic_types::{NodeId, NodeIndex, RegistryVersion};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::sync::Arc;

pub fn create_transcript<C: CspIDkgProtocol + CspSigner>(
    csp_client: &C,
    registry: &Arc<dyn RegistryClient>,
    params: &IDkgTranscriptParams,
    dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
) -> Result<IDkgTranscript, IDkgCreateTranscriptError> {
    ensure_sufficient_dealings_collected(params, dealings)?;
    ensure_dealers_allowed_by_params(params, dealings)?;
    ensure_signers_allowed_by_params(params, dealings)?;
    ensure_sufficient_signatures_collected(params, dealings)?;
    verify_multisignatures(csp_client, registry, dealings, params.registry_version())?;

    let signed_dealings_by_index = dealings_by_index_from_dealings(dealings, params)?;

    let internal_dealings = internal_dealings_from_signed_dealings(&signed_dealings_by_index)?;

    let internal_operation_type =
        IDkgTranscriptOperationInternal::try_from(params.operation_type()).map_err(|e| {
            IDkgCreateTranscriptError::SerializationError {
                internal_error: format!("{:?}", e),
            }
        })?;

    let internal_transcript = csp_client.idkg_create_transcript(
        params.algorithm_id(),
        params.reconstruction_threshold(),
        &internal_dealings,
        &internal_operation_type,
    )?;

    let internal_transcript_raw = internal_transcript.serialize().map_err(|e| {
        IDkgCreateTranscriptError::SerializationError {
            internal_error: format!("{:?}", e),
        }
    })?;

    let transcript_type = IDkgTranscriptType::from(params.operation_type());

    Ok(IDkgTranscript {
        transcript_id: params.transcript_id(),
        receivers: params.receivers().clone(),
        registry_version: params.registry_version(),
        verified_dealings: signed_dealings_by_index,
        transcript_type,
        algorithm_id: params.algorithm_id(),
        internal_transcript_raw,
    })
}

pub fn verify_transcript<C: CspIDkgProtocol + CspSigner>(
    csp_client: &C,
    registry: &Arc<dyn RegistryClient>,
    params: &IDkgTranscriptParams,
    transcript: &IDkgTranscript,
) -> Result<(), IDkgVerifyTranscriptError> {
    transcript
        .verify_consistency_with_params(params)
        .map_err(|e| {
            IDkgVerifyTranscriptError::InvalidArgument(format!(
                "failed to verify transcript against params: {}",
                e
            ))
        })?;

    for (dealer_index, signed_dealing) in &transcript.verified_dealings {
        if signed_dealing.signers.len() < transcript.verification_threshold().get() as usize {
            return Err(IDkgVerifyTranscriptError::InvalidArgument(format!(
                "insufficient number of signers ({}<{}) for dealing of dealer with index {}",
                signed_dealing.signers.len(),
                transcript.verification_threshold(),
                dealer_index
            )));
        }
        // Note that signer eligibility is checked in `transcript.verify_consistency_with_params`
        MultiSigVerifierInternal::verify_multi_sig_combined(
            csp_client,
            Arc::clone(registry),
            &signed_dealing.signature,
            &signed_dealing.signed_dealing,
            signed_dealing.signers.clone(),
            params.registry_version(),
        )
        .map_err(|crypto_error| {
            IDkgVerifyTranscriptError::InvalidDealingMultiSignature {
                error: format!(
                    "invalid combined multi-signature of dealing of dealer with index {}: {}",
                    dealer_index, crypto_error
                ),
                crypto_error,
            }
        })?;
    }

    let internal_transcript_operation =
        IDkgTranscriptOperationInternal::try_from(params.operation_type()).map_err(|e| {
            IDkgVerifyTranscriptError::InvalidArgument(format!(
                "failed to convert transcript operation to internal counterpart: {:?}",
                e
            ))
        })?;
    let internal_transcript = IDkgTranscriptInternal::try_from(transcript).map_err(|e| {
        IDkgVerifyTranscriptError::SerializationError(format!(
            "failed to deserialize internal transcript: {:?}",
            e
        ))
    })?;
    let internal_dealings = internal_dealings_from_verified_dealings(&transcript.verified_dealings)
        .map_err(|e| IDkgVerifyTranscriptError::SerializationError(e.serde_error))?;

    csp_client.idkg_verify_transcript(
        &internal_transcript,
        transcript.algorithm_id,
        params.reconstruction_threshold(),
        &internal_dealings,
        &internal_transcript_operation,
    )
}

pub fn load_transcript<C: CspIDkgProtocol>(
    csp_client: &C,
    self_node_id: &NodeId,
    registry: &Arc<dyn RegistryClient>,
    transcript: &IDkgTranscript,
) -> Result<Vec<IDkgComplaint>, IDkgLoadTranscriptError> {
    let self_index = match transcript.receivers.position(*self_node_id) {
        Some(index) => index,
        None => {
            return Ok(vec![]); // This is not a receiver: nothing to do.
        }
    };

    let self_mega_pubkey = get_mega_pubkey(self_node_id, registry, transcript.registry_version)?;

    let internal_dealings = internal_dealings_from_verified_dealings(&transcript.verified_dealings)
        .map_err(|e| IDkgLoadTranscriptError::SerializationError {
            internal_error: e.serde_error,
        })?;
    let internal_transcript = IDkgTranscriptInternal::try_from(transcript).map_err(|e| {
        IDkgLoadTranscriptError::SerializationError {
            internal_error: format!("{:?}", e),
        }
    })?;
    let internal_complaints = csp_client.idkg_load_transcript(
        &internal_dealings,
        &transcript.context_data(),
        self_index,
        &self_mega_pubkey,
        &internal_transcript,
    )?;
    let complaints = complaints_from_internal_complaints(&internal_complaints, transcript)?;

    Ok(complaints)
}

pub fn load_transcript_with_openings<C: CspIDkgProtocol>(
    csp_client: &C,
    self_node_id: &NodeId,
    registry: &Arc<dyn RegistryClient>,
    transcript: &IDkgTranscript,
    openings: &BTreeMap<IDkgComplaint, BTreeMap<NodeId, IDkgOpening>>,
) -> Result<(), IDkgLoadTranscriptError> {
    let self_index = match transcript.receivers.position(*self_node_id) {
        Some(index) => index,
        None => {
            return Ok(()); // This is not a receiver: nothing to do.
        }
    };
    ensure_sufficient_openings(openings, transcript)?;
    ensure_matching_transcript_ids_and_dealer_ids(openings, transcript)?;

    let self_mega_pubkey = get_mega_pubkey(self_node_id, registry, transcript.registry_version)?;

    let internal_dealings = internal_dealings_from_verified_dealings(&transcript.verified_dealings)
        .map_err(|e| IDkgLoadTranscriptError::SerializationError {
            internal_error: e.serde_error,
        })?;
    let internal_transcript = IDkgTranscriptInternal::try_from(transcript).map_err(|e| {
        IDkgLoadTranscriptError::SerializationError {
            internal_error: format!("{:?}", e),
        }
    })?;

    let mut internal_openings = BTreeMap::new();
    for (complaint, openings_by_opener_id) in openings {
        let mut internal_openings_by_opener_index = BTreeMap::new();
        for (opener_id, opening) in openings_by_opener_id {
            let opener_index = transcript.receivers.position(*opener_id).ok_or_else(|| {
                IDkgLoadTranscriptError::InvalidArguments {
                    internal_error: format!(
                        "invalid opener: node with ID {:?} is not a receiver",
                        *opener_id
                    ),
                }
            })?;
            let internal_opening = CommitmentOpening::try_from(opening).map_err(|e| {
                IDkgLoadTranscriptError::SerializationError {
                    internal_error: format!("failed to deserialize opening: {:?}", e),
                }
            })?;
            internal_openings_by_opener_index.insert(opener_index, internal_opening);
        }
        let dealer_index = transcript
            .index_for_dealer_id(complaint.dealer_id)
            .ok_or_else(|| IDkgLoadTranscriptError::InvalidArguments {
                internal_error: format!(
                    "invalid complaint: node with ID {:?} is not a dealer",
                    complaint.dealer_id
                ),
            })?;
        internal_openings.insert(dealer_index, internal_openings_by_opener_index);
    }

    csp_client.idkg_load_transcript_with_openings(
        &internal_dealings,
        &internal_openings,
        &transcript.context_data(),
        self_index,
        &self_mega_pubkey,
        &internal_transcript,
    )
}

pub fn open_transcript<C: CspIDkgProtocol>(
    csp_idkg_client: &C,
    self_node_id: &NodeId,
    registry: &Arc<dyn RegistryClient>,
    transcript: &IDkgTranscript,
    complainer_id: NodeId,
    complaint: &IDkgComplaint,
) -> Result<IDkgOpening, IDkgOpenTranscriptError> {
    // Verifies the complaint
    verify_complaint(
        csp_idkg_client,
        registry,
        transcript,
        complaint,
        complainer_id,
    )
    .map_err(|e| IDkgOpenTranscriptError::InternalError {
        internal_error: format!("Complaint verification failed: {:?}", e),
    })?;

    // Get the MEGa-encryption public key.
    let opener_public_key = get_mega_pubkey(self_node_id, registry, transcript.registry_version)?;

    // Extract the accused dealing from the transcript.
    let (dealer_index, internal_dealing) =
        index_and_dealing_of_dealer(complaint.dealer_id, transcript)?;
    let context_data = transcript.context_data();
    let opener_index = match transcript.receivers.position(*self_node_id) {
        None => {
            return Err(IDkgOpenTranscriptError::InternalError {
                internal_error: "This node is not a receiver of the given transcript".to_string(),
            })
        }
        Some(index) => index,
    };

    let internal_opening = csp_idkg_client.idkg_open_dealing(
        internal_dealing,
        dealer_index,
        &context_data,
        opener_index,
        &opener_public_key,
    )?;
    let internal_opening_raw =
        internal_opening
            .serialize()
            .map_err(|e| IDkgOpenTranscriptError::InternalError {
                internal_error: format!("Error serializing opening: {:?}", e),
            })?;

    Ok(IDkgOpening {
        transcript_id: transcript.transcript_id,
        dealer_id: complaint.dealer_id,
        internal_opening_raw,
    })
}

pub fn verify_opening<C: CspIDkgProtocol>(
    csp_idkg_client: &C,
    transcript: &IDkgTranscript,
    opener_id: NodeId,
    opening: &IDkgOpening,
    complaint: &IDkgComplaint,
) -> Result<(), IDkgVerifyOpeningError> {
    // Check ID of transcript inside the complaint
    if (complaint.transcript_id != transcript.transcript_id)
        || (opening.transcript_id != transcript.transcript_id)
    {
        return Err(IDkgVerifyOpeningError::TranscriptIdMismatch);
    }

    if opening.dealer_id != complaint.dealer_id {
        return Err(IDkgVerifyOpeningError::DealerIdMismatch);
    }

    // Extract the accused dealing from the transcript
    let (_, internal_dealing) = index_and_dealing_of_dealer(complaint.dealer_id, transcript)?;
    let opener_index = transcript
        .receivers
        .position(opener_id)
        .ok_or(IDkgVerifyOpeningError::MissingOpenerInReceivers { opener_id })?;
    let internal_opening = CommitmentOpening::try_from(opening).map_err(|e| {
        IDkgVerifyOpeningError::InternalError {
            internal_error: format!("Failed to deserialize opening: {:?}", e),
        }
    })?;

    csp_idkg_client.idkg_verify_dealing_opening(internal_dealing, opener_index, internal_opening)
}

fn ensure_sufficient_dealings_collected(
    params: &IDkgTranscriptParams,
    dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
) -> Result<(), IDkgCreateTranscriptError> {
    if dealings.len() < params.collection_threshold().get() as usize {
        Err(IDkgCreateTranscriptError::UnsatisfiedCollectionThreshold {
            threshold: params.collection_threshold().get(),
            dealing_count: dealings.len(),
        })
    } else {
        Ok(())
    }
}

fn ensure_dealers_allowed_by_params(
    params: &IDkgTranscriptParams,
    dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
) -> Result<(), IDkgCreateTranscriptError> {
    for id in dealings.keys() {
        if !params.dealers().get().contains(id) {
            return Err(IDkgCreateTranscriptError::DealerNotAllowed { node_id: *id });
        }
    }

    Ok(())
}

fn ensure_signers_allowed_by_params(
    params: &IDkgTranscriptParams,
    dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
) -> Result<(), IDkgCreateTranscriptError> {
    for dealing in dealings.values() {
        for signer in &dealing.signers {
            if !params.receivers().get().contains(signer) {
                return Err(IDkgCreateTranscriptError::SignerNotAllowed { node_id: *signer });
            }
        }
    }

    Ok(())
}

fn ensure_sufficient_signatures_collected(
    params: &IDkgTranscriptParams,
    dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
) -> Result<(), IDkgCreateTranscriptError> {
    for (dealer, dealing) in dealings {
        if dealing.signers.len() < params.verification_threshold().get() as usize {
            return Err(
                IDkgCreateTranscriptError::UnsatisfiedVerificationThreshold {
                    threshold: params.verification_threshold().get(),
                    signature_count: dealing.signers.len(),
                    dealer_id: *dealer,
                },
            );
        }
    }

    Ok(())
}

fn verify_multisignatures<C: CspSigner>(
    csp_client: &C,
    registry: &Arc<dyn RegistryClient>,
    dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
    registry_version: RegistryVersion,
) -> Result<(), IDkgCreateTranscriptError> {
    for dealing in dealings.values() {
        MultiSigVerifierInternal::verify_multi_sig_combined(
            csp_client,
            Arc::clone(registry),
            &dealing.signature,
            &dealing.signed_dealing,
            dealing.signers.clone(),
            registry_version,
        )
        .map_err(|e| IDkgCreateTranscriptError::InvalidMultisignature { crypto_error: e })?;
    }

    Ok(())
}

/// Convert values in the dealings map from IDkgDealings to IDkgDealingInternals
fn internal_dealings_from_signed_dealings(
    dealings: &BTreeMap<NodeIndex, IDkgMultiSignedDealing>,
) -> Result<BTreeMap<NodeIndex, IDkgDealingInternal>, IDkgCreateTranscriptError> {
    dealings
        .iter()
        .map(|(index, d)| {
            let internal_dealing =
                IDkgDealingInternal::deserialize(&d.signed_dealing.content.internal_dealing_raw)
                    .map_err(|e| IDkgCreateTranscriptError::SerializationError {
                        internal_error: format!("{:?}", e),
                    })?;
            Ok((*index, internal_dealing))
        })
        .collect()
}

/// Re-key the dealings map to use indices rather than ids
///
/// The indices are such that they allow the previous transcript(s) (if any)
/// to be properly recombined (i.e. the indices are for the previous sharing,
/// if this is a resharing or multiplication).
///
/// Only the first collection_threshold dealings are returned
fn dealings_by_index_from_dealings(
    dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
    params: &IDkgTranscriptParams,
) -> Result<BTreeMap<NodeIndex, IDkgMultiSignedDealing>, IDkgCreateTranscriptError> {
    dealings
        .iter()
        .take(params.collection_threshold().get() as usize)
        .map(|(id, d)| {
            let index = params
                .dealer_index(*id)
                .ok_or(IDkgCreateTranscriptError::DealerNotAllowed { node_id: *id })?;
            Ok((index, d.clone()))
        })
        .collect()
}

fn internal_dealings_from_verified_dealings(
    verified_dealings: &BTreeMap<NodeIndex, IDkgMultiSignedDealing>,
) -> Result<
    BTreeMap<NodeIndex, IDkgDealingInternal>,
    InternalDealingsFromVerifiedDealingsSerializationError,
> {
    verified_dealings
        .iter()
        .map(|(index, signed_dealing)| {
            let dealing = IDkgDealingInternal::try_from(signed_dealing).map_err(|e| {
                InternalDealingsFromVerifiedDealingsSerializationError {
                    serde_error: format!("failed to deserialize internal dealing: {:?}", e),
                }
            })?;
            Ok((*index, dealing))
        })
        .collect()
}

struct InternalDealingsFromVerifiedDealingsSerializationError {
    serde_error: String,
}

/// Builds IDkgComplaint's from IDkgComplaintInternal's
/// (which translates a dealer's NodeIndex to a NodeId)
fn complaints_from_internal_complaints(
    internal_complaints: &BTreeMap<NodeIndex, IDkgComplaintInternal>,
    transcript: &IDkgTranscript,
) -> Result<Vec<IDkgComplaint>, IDkgLoadTranscriptError> {
    internal_complaints
        .iter()
        .map(|(dealer_index, internal_complaint)| {
            let internal_complaint_raw = internal_complaint.serialize().map_err(|e| {
                IDkgLoadTranscriptError::SerializationError {
                    internal_error: format!("{:?}", e),
                }
            })?;
            let dealer_id = transcript
                .dealer_id_for_index(*dealer_index)
                .ok_or_else(|| IDkgLoadTranscriptError::InternalError {
                    internal_error: format!("failed to get dealer ID for index {}", dealer_index),
                })?;

            Ok(IDkgComplaint {
                transcript_id: transcript.transcript_id,
                dealer_id,
                internal_complaint_raw,
            })
        })
        .collect()
}

fn ensure_sufficient_openings(
    openings: &BTreeMap<IDkgComplaint, BTreeMap<NodeId, IDkgOpening>>,
    transcript: &IDkgTranscript,
) -> Result<(), IDkgLoadTranscriptError> {
    let reconstruction_threshold_usize =
        usize::try_from(transcript.reconstruction_threshold().get()).map_err(|e| {
            IDkgLoadTranscriptError::InternalError {
                internal_error: format!(
                    "failed to convert reconstruction threshold to usize: {:?}",
                    e
                ),
            }
        })?;

    for complaint_openings in openings.values() {
        if complaint_openings.len() < reconstruction_threshold_usize {
            return Err(IDkgLoadTranscriptError::InsufficientOpenings {
                internal_error: format!(
                    "insufficient number of openings: got {}, but required {}",
                    complaint_openings.len(),
                    reconstruction_threshold_usize
                ),
            });
        }
    }
    Ok(())
}

fn ensure_matching_transcript_ids_and_dealer_ids(
    openings: &BTreeMap<IDkgComplaint, BTreeMap<NodeId, IDkgOpening>>,
    transcript: &IDkgTranscript,
) -> Result<(), IDkgLoadTranscriptError> {
    for (complaint, openings_by_opener_id) in openings {
        if complaint.transcript_id != transcript.transcript_id {
            return Err(IDkgLoadTranscriptError::InvalidArguments {
                internal_error: format!(
                    "mismatching transcript IDs in complaint ({:?}) and transcript ({:?})",
                    complaint.transcript_id, transcript.transcript_id
                ),
            });
        }
        for opening in openings_by_opener_id.values() {
            if opening.transcript_id != transcript.transcript_id {
                return Err(IDkgLoadTranscriptError::InvalidArguments {
                    internal_error: format!(
                        "mismatching transcript IDs in opening ({:?}) and transcript ({:?})",
                        opening.transcript_id, transcript.transcript_id
                    ),
                });
            }
            if opening.dealer_id != complaint.dealer_id {
                return Err(IDkgLoadTranscriptError::InvalidArguments {
                    internal_error: format!(
                        "mismatching dealer IDs in opening ({:?}) and the complaint ({:?})",
                        opening.dealer_id, complaint.dealer_id
                    ),
                });
            }
        }
    }
    Ok(())
}
