//! This module provides the component responsible for generating and validating
//! payloads relevant to threshold ECDSA signatures.
//!
//! # Goal of threshold ECDSA
//! We want canisters to be able to hold BTC, ETH, and for them to create
//! bitcoin and ethereum transactions. Since those networks use ECDSA, a
//! canister must be able to create ECDSA signatures. Since a canister cannot
//! hold the secret key itself, the secret key will be shared among the replicas
//! of the subnet, and they must be able to collaboratively create ECDSA
//! signatures.
//!
//! # High level implementation design
//! Each subnet will have a single threshold ECDSA key. From this key, we will
//! derive per-canister keys. A canister can via a system API request an ECDSA
//! signature, and this request is stored in the replicated state. Consensus
//! will observe these requests and store in blocks which signatures should be
//! created.
//!
//! ## Distributed Key Generation & Transcripts
//! To create threshold ECDSA signatures we need a `Transcript` that gives all
//! replicas shares of an ECDSA secret key. However, this is not sufficient: we
//! need additional transcripts to share the ephemeral values used in an ECDSA
//! signature. The creation of one ECDSA signature requires a transcript that
//! shares the ECDSA signing key `x`, and additionally four DKG transcripts,
//! with a special structure: we need transcripts `t1`, `t2`, `t3`, `t4`, such
//! that `t1` and `t2` share a random values `r1` and `r2` respectively, `t3`
//! shares the product `r1 * r2`, and `t4` shares `r2 * x`.
//!
//! Such transcripts are created via a distributed key generation (DKG)
//! protocol. The DKG for these transcripts must be computationally efficient,
//! because we need four transcripts per signature, and we want to be able to
//! create many signatures. This means that we need interactive DKG for ECDSA
//! related things, instead of non-interactive DKG like we do for our threshold
//! BLS signatures.
//!
//! Consensus orchestrates the creation of these transcripts. Blocks contain
//! configs indicating which transcripts should be created. Such configs come in
//! different types, because some transcripts should share a random value, while
//! others need to share the product of two other transcripts. Complete
//! transcripts will be included in blocks via the functions
//! [create_data_payload] and [create_summary_payload].
//!
//! # [EcdsaImpl] behavior
//! The ECDSA component is responsible for adding artifacts to the ECDSA
//! artifact pool, and validating artifacts in that pool, by exposing a function
//! `on_state_change`. This function behaves as follows, where `finalized_tip`
//! denotes the latest finalized consensus block.
//!
//! ## add DKG dealings
//! for every config in `finalized_tip.ecdsa.configs`, do the following: if this
//! replica is a dealer in this config, and no dealing for this config created
//! by this replica is in the validated pool,then create a dealing for this
//! config, and add it to the validated pool
//!
//! ## validate DKG dealings
//! for every unvalidated dealing d, do the following. If `d.config_id` is an
//! element of `finalized_tip.ecdsa.configs`, the validated pool does not yet
//! contain a dealing from `d.dealer` for `d.config_id`, then do the public
//! cryptographic validation of the dealing, and move it to the validated pool
//! if valid, or remove it from the unvalidated pool if invalid.
//!
//! ## Support DKG dealings
//! In the previous step, we only did the "public" verification of the dealings,
//! which does not check that the dealing encrypts a good share for this
//! replica. For every validated dealing d for which no support message by this
//! replica exists in the validated pool, do the "private" cryptographic
//! validation, and if valid, add a support dealing message for d to the
//! validated pool.
//!
//! ## Remove stale dealings
//! for every validated or unvalidated dealing d, do the following. If
//! `d.config_id` is not an element of `finalized_tip.ecdsa.configs`, and
//! `d.config_id` is older than `finalized_tip`, remove `d` from the pool.
//!
//! ## add signature shares
//! for every signature request `req` in
//! `finalized_tip.ecdsa.signature_requests`, do the following: if this replica
//! is a signer for `req` and no signature share by this replica is in the
//! validated pool, create a signature share for `req` and add it to the
//! validated pool.
//!
//! ## validate signature shares
//! for every unvalidated signature share s, do the following: if `s.config_id`
//! is an element of `finalized_tip.ecdsa.configs`, and there is no signature
//! share by `s.signer` for `s.config_id` in the validated pool yet, then
//! cryptographically validate the signature share. If valid, move `s` to
//! validated, and if invalid, remove `s` from unvalidated.
//!
//! ## aggregate ECDSA signatures
//! For every signature request `req` in
//! `finalized_tip.ecdsa.signature_requests` for which no signature is present
//! in the validated pool, do the following: if there are at least
//! `req.threshold` signature shares wrt `req.config` from distinct signers in
//! the validated pool, aggregate the shares into a full ECDSA signature, and
//! add this signature to the validated pool.
//!
//! ## validate full ECDSA signature
//! // TODO
//!
//! ## complaints & openings
//! // TODO
//!
//! # ECDSA payload on blocks
//! The ECDSA payload on blocks serves some purposes: it should ensure that all
//! replicas are doing DKGs to help create the transcripts required for more
//! 4-tuples which are used to create ECDSA signatures. In addition, it should
//! match signature requests to available 4-tuples and generate signatures.
//!
//! Every block contains
//! - a set of "4-tuples being created"
//! - a set of "available 4-tuples"
//! - a set of "ongoing signing requests", which pair signing requests with
//!   4-tuples
//! - newly finished signatures to deliver up
//!
//! The "4 tuples in creation" contain the following information
//! - kappa_config: config for 1st masked random transcript
//! - optionally, kappa_masked: transcript resulting from kappa_config
//! - lambda_config: config for 2nd masked random transcript
//! - optionally, lambda_masked: transcript resulting from kappa_config
//! - optionally, unmask_kappa_config: config for resharing as unmasked of
//!   kappa_masked
//! - optionally, kappa_unmasked: transcript resulting from unmask_kappa_config
//! - optionally, key_times_lambda_config: multiplication of the ECDSA secret
//!   key and lambda_masked transcript (so masked multiplication of unmasked and
//!   masked)
//! - optionally, key_times_lambda: transcript resulting from
//!   key_times_lambda_config
//! - optionally, kappa_times_lambda_config: config of multiplication
//!   kappa_unasmked and lambda_masked (so masked multiplication of unmasked and
//!   masked)
//! - optionally, kappa_times_lambda: transcript resulting from
//!   kappa_times_lambda_config
//!
//! The relation between the different configs/transcripts can be summarized as
//! follows:
//! ```text
//! kappa_masked ────────► kappa_unmasked ─────────►
//!                                                 kappa_times_lambda
//!         ┌──────────────────────────────────────►
//!         │
//! lambda_masked
//!         │
//!         └───────────►
//!                        key_times_lambda
//! ecdsa_key  ─────────►
//! ```
//! The data transforms like a state machine:
//! - remove all signature requests from "ongoing signature requests" that are
//!   no longer present in the replicated state (referenced via the validation
//!   context)
//! - when a new transcript is complete, it is added to the corresponding
//!   "4-tuple being created"
//!     - when kappa_masked is set, unmask_kappa_config should be set (reshare
//!       to unmask)
//!     - when lambda_masked is set, key_times_lambda_config should be set
//!     - when lambda_masked and kappa_unmasked are set,
//!       kappa_times_lambda_config must be set
//!     - when kappa_unmasked, lambda_masked, key_times_lambda,
//!       kappa_times_lambda are set, the tuple should no longer be in "in
//!       creation", but instead be moved to the complete 4-tuples.
//! - whenever the state lists a new signature request (for which no "ongoing
//!   signing request" is present) and available 4-tuples is not empty, remove
//!   the first 4-tuple from the available 4 tuples and make an entry in ongoing
//!   signatures with the signing request and the 4-tuple.
// TODO: Remove after implementing functionality
#![allow(dead_code)]

use crate::consensus::{
    metrics::{timed_call, EcdsaClientMetrics},
    utils::RoundRobin,
    ConsensusCrypto,
};
use crate::ecdsa::complaints::{EcdsaComplaintHandler, EcdsaComplaintHandlerImpl};
use crate::ecdsa::pre_signer::{EcdsaPreSigner, EcdsaPreSignerImpl};
use crate::ecdsa::signer::{EcdsaSigner, EcdsaSignerImpl};
use crate::ecdsa::utils::EcdsaBlockReaderImpl;

use ic_interfaces::consensus_pool::ConsensusBlockCache;
use ic_interfaces::ecdsa::{Ecdsa, EcdsaChangeSet, EcdsaGossip, EcdsaPool};
use ic_logger::ReplicaLogger;
use ic_metrics::MetricsRegistry;
use ic_types::{
    artifact::{EcdsaMessageAttribute, EcdsaMessageId, Priority, PriorityFn},
    consensus::ecdsa::EcdsaBlockReader,
    malicious_flags::MaliciousFlags,
    Height, NodeId, SubnetId,
};

use std::sync::Arc;

pub(crate) mod complaints;
pub(crate) mod payload_builder;
pub(crate) mod payload_verifier;
pub(crate) mod pre_signer;
pub(crate) mod signer;
pub(crate) mod utils;

pub use payload_builder::make_bootstrap_summary;
pub(crate) use payload_builder::{create_data_payload, create_summary_payload};
pub(crate) use payload_verifier::{validate_payload, PermanentError, TransientError};

/// Similar to consensus, we don't fetch artifacts too far ahead in future.
const LOOK_AHEAD: u64 = 10;

/// `EcdsaImpl` is the consensus component responsible for processing threshold
/// ECDSA payloads.
pub struct EcdsaImpl {
    subnet_id: SubnetId,
    pre_signer: Box<dyn EcdsaPreSigner>,
    signer: Box<dyn EcdsaSigner>,
    complaint_handler: Box<dyn EcdsaComplaintHandler>,
    schedule: RoundRobin,
    metrics: EcdsaClientMetrics,
    logger: ReplicaLogger,
}

impl EcdsaImpl {
    /// Builds a new threshold ECDSA component
    pub fn new(
        node_id: NodeId,
        subnet_id: SubnetId,
        consensus_block_cache: Arc<dyn ConsensusBlockCache>,
        crypto: Arc<dyn ConsensusCrypto>,
        metrics_registry: MetricsRegistry,
        logger: ReplicaLogger,
        malicious_flags: MaliciousFlags,
    ) -> Self {
        let pre_signer = Box::new(EcdsaPreSignerImpl::new(
            node_id,
            subnet_id,
            consensus_block_cache.clone(),
            crypto.clone(),
            metrics_registry.clone(),
            logger.clone(),
            malicious_flags,
        ));
        let signer = Box::new(EcdsaSignerImpl::new(
            node_id,
            consensus_block_cache.clone(),
            crypto.clone(),
            metrics_registry.clone(),
            logger.clone(),
        ));
        let complaint_handler = Box::new(EcdsaComplaintHandlerImpl::new(
            node_id,
            consensus_block_cache,
            crypto,
            metrics_registry.clone(),
            logger.clone(),
        ));
        Self {
            subnet_id,
            pre_signer,
            signer,
            complaint_handler,
            schedule: RoundRobin::default(),
            metrics: EcdsaClientMetrics::new(metrics_registry),
            logger,
        }
    }
}

impl Ecdsa for EcdsaImpl {
    fn on_state_change(&self, ecdsa_pool: &dyn EcdsaPool) -> EcdsaChangeSet {
        let metrics = self.metrics.clone();
        let pre_signer = || {
            timed_call(
                "pre_signer",
                || {
                    self.pre_signer
                        .on_state_change(ecdsa_pool, self.complaint_handler.as_transcript_loader())
                },
                &metrics.on_state_change_duration,
            )
        };
        let signer = || {
            timed_call(
                "signer",
                || {
                    self.signer
                        .on_state_change(ecdsa_pool, self.complaint_handler.as_transcript_loader())
                },
                &metrics.on_state_change_duration,
            )
        };
        let complaint_handler = || {
            timed_call(
                "complaint_handler",
                || self.complaint_handler.on_state_change(ecdsa_pool),
                &metrics.on_state_change_duration,
            )
        };

        let calls: [&'_ dyn Fn() -> EcdsaChangeSet; 3] = [&pre_signer, &signer, &complaint_handler];
        self.schedule.call_next(&calls)
    }
}

/// `EcdsaGossipImpl` implements the priority function and other gossip related
/// functionality
pub struct EcdsaGossipImpl {
    subnet_id: SubnetId,
    consensus_block_cache: Arc<dyn ConsensusBlockCache>,
}

impl EcdsaGossipImpl {
    /// Builds a new EcdsaGossipImpl component
    pub fn new(subnet_id: SubnetId, consensus_block_cache: Arc<dyn ConsensusBlockCache>) -> Self {
        Self {
            subnet_id,
            consensus_block_cache,
        }
    }
}

impl EcdsaGossip for EcdsaGossipImpl {
    fn get_priority_function(
        &self,
        _ecdsa_pool: &dyn EcdsaPool,
    ) -> PriorityFn<EcdsaMessageId, EcdsaMessageAttribute> {
        let block_reader = EcdsaBlockReaderImpl::new(self.consensus_block_cache.finalized_chain());
        let subnet_id = self.subnet_id;
        let cached_finalized_height = block_reader.tip_height();
        Box::new(move |_, attr: &'_ EcdsaMessageAttribute| {
            compute_priority(attr, subnet_id, cached_finalized_height)
        })
    }
}

// TODO:
// 1. We don't drop anything right now. Once the purging part settles down
// (https://dfinity.atlassian.net/browse/CON-624), we can start dropping
// unwanted adverts
// 2. The current filtering is light weight, purely based on the finalized
// height (cached when the priority function is periodically computed).
// We could potentially do more filtering (e.g) drop adverts for transcripts
// we are not interested in, and use the latest state of the artifact pools.
// But this would require more processing per call to priority function, and
// cause extra lock contention for the main processing paths.
// 3. The height in the dealings/support shares is the source subnet height, which
// may be different in the xnet resharing case. Local requested height is not available
// in these cases. Hence fetch them immediately. The requested_height will be removed
// for other artifact types as well in near future.
fn compute_priority(
    attr: &EcdsaMessageAttribute,
    subnet_id: SubnetId,
    cached_finalized_height: Height,
) -> Priority {
    let height = match attr {
        EcdsaMessageAttribute::EcdsaSignedDealing(transcript_id)
        | EcdsaMessageAttribute::EcdsaDealingSupport(transcript_id) => {
            // For xnet dealings(target side), always fetch the artifacts,
            // as the source_height from different subnet cannot be compared
            // anyways.
            if *transcript_id.source_subnet() != subnet_id {
                return Priority::Fetch;
            }
            transcript_id.source_height()
        }
        EcdsaMessageAttribute::EcdsaSigShare(height) => *height,
        EcdsaMessageAttribute::EcdsaComplaint(height) => *height,
        EcdsaMessageAttribute::EcdsaOpening(height) => *height,
    };

    if height < cached_finalized_height + Height::from(LOOK_AHEAD) {
        Priority::Fetch
    } else {
        Priority::Stash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_types::crypto::canister_threshold_sig::idkg::IDkgTranscriptId;
    use ic_types::{PrincipalId, SubnetId};

    // Tests the priority computation
    #[test]
    fn test_ecdsa_priority_fn() {
        let cached_finalized_height = Height::from(100);
        let xnet_subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(1));
        let subnet_id = SubnetId::from(PrincipalId::new_subnet_test_id(2));
        let xnet_transcript_id = IDkgTranscriptId::new(xnet_subnet_id, 1, Height::from(1000));
        let local_transcript_id = IDkgTranscriptId::new(subnet_id, 1, Height::from(120));
        let tests = vec![
            (
                EcdsaMessageAttribute::EcdsaSignedDealing(xnet_transcript_id),
                Priority::Fetch,
            ),
            (
                EcdsaMessageAttribute::EcdsaDealingSupport(xnet_transcript_id),
                Priority::Fetch,
            ),
            (
                EcdsaMessageAttribute::EcdsaSignedDealing(local_transcript_id),
                Priority::Stash,
            ),
            (
                EcdsaMessageAttribute::EcdsaDealingSupport(local_transcript_id),
                Priority::Stash,
            ),
            (
                EcdsaMessageAttribute::EcdsaSigShare(Height::from(90)),
                Priority::Fetch,
            ),
            (
                EcdsaMessageAttribute::EcdsaSigShare(Height::from(109)),
                Priority::Fetch,
            ),
            (
                EcdsaMessageAttribute::EcdsaComplaint(Height::from(110)),
                Priority::Stash,
            ),
            (
                EcdsaMessageAttribute::EcdsaComplaint(Height::from(120)),
                Priority::Stash,
            ),
            (
                EcdsaMessageAttribute::EcdsaOpening(Height::from(90)),
                Priority::Fetch,
            ),
            (
                EcdsaMessageAttribute::EcdsaOpening(Height::from(109)),
                Priority::Fetch,
            ),
            (
                EcdsaMessageAttribute::EcdsaOpening(Height::from(110)),
                Priority::Stash,
            ),
            (
                EcdsaMessageAttribute::EcdsaOpening(Height::from(120)),
                Priority::Stash,
            ),
        ];

        for (attr, expected) in tests {
            assert_eq!(
                compute_priority(&attr, subnet_id, cached_finalized_height),
                expected
            );
        }
    }
}
