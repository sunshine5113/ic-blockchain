//! The pre signature process manager

use crate::consensus::{
    metrics::{timed_call, EcdsaPayloadMetrics, EcdsaPreSignerMetrics},
    utils::RoundRobin,
    ConsensusCrypto,
};
use crate::ecdsa::complaints::EcdsaTranscriptLoader;
use crate::ecdsa::utils::{load_transcripts, transcript_op_summary, EcdsaBlockReaderImpl};
use ic_interfaces::consensus_pool::ConsensusBlockCache;
use ic_interfaces::crypto::{ErrorReplication, IDkgProtocol};
use ic_interfaces::ecdsa::{EcdsaChangeAction, EcdsaChangeSet, EcdsaPool};
use ic_logger::{debug, warn, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_types::artifact::EcdsaMessageId;
use ic_types::consensus::ecdsa::{EcdsaBlockReader, EcdsaMessage};
use ic_types::crypto::canister_threshold_sig::idkg::{
    IDkgDealing, IDkgDealingSupport, IDkgMultiSignedDealing, IDkgTranscript, IDkgTranscriptId,
    IDkgTranscriptOperation, IDkgTranscriptParams, SignedIDkgDealing, VerifiedIdkgDealing,
};
use ic_types::malicious_flags::MaliciousFlags;
use ic_types::signature::MultiSignature;
use ic_types::{Height, NodeId, SubnetId};

use prometheus::IntCounterVec;
use std::cell::RefCell;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

pub(crate) trait EcdsaPreSigner: Send {
    /// The on_state_change() called from the main ECDSA path.
    fn on_state_change(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_loader: &dyn EcdsaTranscriptLoader,
    ) -> EcdsaChangeSet;
}

pub(crate) struct EcdsaPreSignerImpl {
    node_id: NodeId,
    subnet_id: SubnetId,
    consensus_block_cache: Arc<dyn ConsensusBlockCache>,
    crypto: Arc<dyn ConsensusCrypto>,
    schedule: RoundRobin,
    metrics: EcdsaPreSignerMetrics,
    log: ReplicaLogger,
    malicious_flags: MaliciousFlags,
}

impl EcdsaPreSignerImpl {
    pub(crate) fn new(
        node_id: NodeId,
        subnet_id: SubnetId,
        consensus_block_cache: Arc<dyn ConsensusBlockCache>,
        crypto: Arc<dyn ConsensusCrypto>,
        metrics_registry: MetricsRegistry,
        log: ReplicaLogger,
        malicious_flags: MaliciousFlags,
    ) -> Self {
        Self {
            node_id,
            subnet_id,
            consensus_block_cache,
            crypto,
            schedule: RoundRobin::default(),
            metrics: EcdsaPreSignerMetrics::new(metrics_registry),
            log,
            malicious_flags,
        }
    }

    /// Starts the transcript generation sequence by issuing the
    /// dealing for the transcript. The requests for new transcripts
    /// come from the latest summary block
    fn send_dealings(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_loader: &dyn EcdsaTranscriptLoader,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let requested_transcripts = resolve_transcript_refs(
            block_reader,
            "send_dealings",
            self.metrics.pre_sign_errors.clone(),
            &self.log,
        );

        let mut target_subnet_xnet_transcripts = BTreeSet::new();
        for transcript_params_ref in block_reader.target_subnet_xnet_transcripts() {
            target_subnet_xnet_transcripts.insert(transcript_params_ref.transcript_id);
        }

        requested_transcripts
            .iter()
            .filter(|transcript_params| {
                // Issue a dealing if we are in the dealer list and we haven't
                //already issued a dealing for this transcript
                transcript_params.dealers().position(self.node_id).is_some()
                    && !self.has_dealer_issued_dealing(
                        ecdsa_pool,
                        &transcript_params.transcript_id(),
                        &self.node_id,
                    )
            })
            .flat_map(|transcript_params| {
                if target_subnet_xnet_transcripts.contains(&transcript_params.transcript_id()) {
                    self.metrics
                        .pre_sign_errors_inc("create_dealing_for_xnet_transcript");
                    warn!(
                        self.log,
                        "Dealing creation: dealing for target xnet dealing: {:?}",
                        transcript_params,
                    );
                }

                self.crypto_create_dealing(
                    ecdsa_pool,
                    transcript_loader,
                    block_reader,
                    transcript_params,
                )
            })
            .collect()
    }

    /// Processes the dealings received from peer dealers
    fn validate_dealings(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let requested_transcripts = resolve_transcript_refs(
            block_reader,
            "validate_dealings",
            self.metrics.pre_sign_errors.clone(),
            &self.log,
        );

        // Pass 1: collection of <TranscriptId, DealerId>
        let mut dealing_keys = BTreeSet::new();
        let mut duplicate_keys = BTreeSet::new();
        for (_, signed_dealing) in ecdsa_pool.unvalidated().signed_dealings() {
            let dealing = signed_dealing.idkg_dealing();
            let key = (dealing.transcript_id, signed_dealing.dealer_id());
            if !dealing_keys.insert(key) {
                duplicate_keys.insert(key);
            }
        }

        let mut target_subnet_xnet_transcripts = BTreeSet::new();
        for transcript_params_ref in block_reader.target_subnet_xnet_transcripts() {
            target_subnet_xnet_transcripts.insert(transcript_params_ref.transcript_id);
        }

        let mut ret = Vec::new();
        for (id, signed_dealing) in ecdsa_pool.unvalidated().signed_dealings() {
            let dealing = signed_dealing.idkg_dealing();
            // Remove the duplicate entries
            let key = (dealing.transcript_id, signed_dealing.dealer_id());
            if duplicate_keys.contains(&key) {
                self.metrics
                    .pre_sign_errors_inc("duplicate_dealing_in_batch");
                ret.push(EcdsaChangeAction::HandleInvalid(
                    id,
                    format!("Duplicate dealing in unvalidated batch: {}", signed_dealing),
                ));
                continue;
            }

            // We don't expect dealings for the xnet transcripts on the target subnet
            // (as the initial dealings are already built and passed in by the source
            // subnet)
            if target_subnet_xnet_transcripts.contains(&dealing.transcript_id) {
                self.metrics
                    .pre_sign_errors_inc("unexpected_dealing_xnet_target_subnet");
                ret.push(EcdsaChangeAction::HandleInvalid(
                    id,
                    format!(
                        "Dealing for xnet dealing on target subnet: {}",
                        signed_dealing
                    ),
                ));
                continue;
            }

            match Action::action(
                block_reader,
                &requested_transcripts,
                Some(dealing.transcript_id.source_height()),
                &dealing.transcript_id,
            ) {
                Action::Process(transcript_params) => {
                    if transcript_params
                        .dealers()
                        .position(signed_dealing.dealer_id())
                        .is_none()
                    {
                        // The node is not in the dealer list for this transcript
                        self.metrics.pre_sign_errors_inc("unexpected_dealing");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!("Dealing from unexpected node: {}", signed_dealing),
                        ))
                    } else if self.has_dealer_issued_dealing(
                        ecdsa_pool,
                        &dealing.transcript_id,
                        &signed_dealing.dealer_id(),
                    ) {
                        // The node already sent a valid dealing for this transcript
                        self.metrics.pre_sign_errors_inc("duplicate_dealing");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!("Duplicate dealing: {}", signed_dealing),
                        ))
                    } else {
                        let mut changes =
                            self.crypto_verify_dealing(&id, transcript_params, &signed_dealing);
                        ret.append(&mut changes);
                    }
                }
                Action::Drop => ret.push(EcdsaChangeAction::RemoveUnvalidated(id)),
                Action::Defer => {}
            }
        }
        ret
    }

    /// Sends out the signature share for the dealings received from peer
    /// dealers
    fn send_dealing_support(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let requested_transcripts = resolve_transcript_refs(
            block_reader,
            "send_dealing_support",
            self.metrics.pre_sign_errors.clone(),
            &self.log,
        );

        // TranscriptId -> TranscriptParams
        let mut trancript_param_map = BTreeMap::new();
        for transcript_params in &requested_transcripts {
            trancript_param_map.insert(transcript_params.transcript_id(), transcript_params);
        }

        let mut source_subnet_xnet_transcripts = BTreeSet::new();
        for transcript_params_ref in block_reader.source_subnet_xnet_transcripts() {
            source_subnet_xnet_transcripts.insert(transcript_params_ref.transcript_id);
        }

        ecdsa_pool
            .validated()
            .signed_dealings()
            .filter(|(_, signed_dealing)| {
                let dealing = signed_dealing.idkg_dealing();
                !self.has_node_issued_dealing_support(
                    ecdsa_pool,
                    &dealing.transcript_id,
                    &signed_dealing.dealer_id(),
                    &self.node_id,
                )
            })
            .filter_map(|(id, signed_dealing)| {
                let dealing = signed_dealing.idkg_dealing();
                // Look up the transcript params for the dealing, and check if we
                // are a receiver for this dealing
                if let Some(transcript_params) = trancript_param_map.get(&dealing.transcript_id) {
                    transcript_params
                        .receivers()
                        .position(self.node_id)
                        .map(|_| (id, transcript_params, signed_dealing))
                } else {
                    self.metrics
                        .pre_sign_errors_inc("create_support_missing_transcript_params");
                    warn!(
                        self.log,
                        "Dealing support creation: transcript_param not found: {}", signed_dealing,
                    );
                    None
                }
            })
            .flat_map(|(id, transcript_params, signed_dealing)| {
                let dealing = signed_dealing.idkg_dealing();
                if source_subnet_xnet_transcripts.contains(&dealing.transcript_id) {
                    self.metrics
                        .pre_sign_errors_inc("create_support_for_xnet_transcript");
                    warn!(
                        self.log,
                        "Dealing support creation: support for target xnet dealing: {}",
                        signed_dealing,
                    );
                }
                self.crypto_create_dealing_support(&id, transcript_params, &signed_dealing)
            })
            .collect()
    }

    /// Processes the received dealing support messages
    fn validate_dealing_support(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let requested_transcripts = resolve_transcript_refs(
            block_reader,
            "validate_dealing_support",
            self.metrics.pre_sign_errors.clone(),
            &self.log,
        );

        // Get the set of valid dealings <TranscriptId, DealerId>
        let mut valid_dealings = BTreeSet::new();
        for (_, signed_dealing) in ecdsa_pool.validated().signed_dealings() {
            let dealing = signed_dealing.idkg_dealing();
            let dealing_key = (dealing.transcript_id, signed_dealing.dealer_id());
            valid_dealings.insert(dealing_key);
        }

        let mut source_subnet_xnet_transcripts = BTreeSet::new();
        for transcript_params_ref in block_reader.source_subnet_xnet_transcripts() {
            source_subnet_xnet_transcripts.insert(transcript_params_ref.transcript_id);
        }

        let mut target_subnet_xnet_transcripts = BTreeSet::new();
        for transcript_params_ref in block_reader.target_subnet_xnet_transcripts() {
            target_subnet_xnet_transcripts.insert(transcript_params_ref.transcript_id);
        }

        // Pass 1: collection of <TranscriptId, DealerId, SignerId>
        let mut supports = BTreeSet::new();
        let mut duplicate_supports = BTreeSet::new();
        for (_, support) in ecdsa_pool.unvalidated().dealing_support() {
            let dealing = &support.content.content;
            let support_key = (
                dealing.transcript_id,
                support.content.dealer_id(),
                support.signature.signer,
            );
            if !supports.insert(support_key) {
                duplicate_supports.insert(support_key);
            }
        }

        let mut ret = Vec::new();
        for (id, support) in ecdsa_pool.unvalidated().dealing_support() {
            let dealing = &support.content.content;
            let dealing_key = (dealing.transcript_id, support.content.dealer_id());
            let support_key = (
                dealing.transcript_id,
                support.content.dealer_id(),
                support.signature.signer,
            );

            // Remove the duplicate entries
            if duplicate_supports.contains(&support_key) {
                self.metrics
                    .pre_sign_errors_inc("duplicate_support_in_batch");
                ret.push(EcdsaChangeAction::HandleInvalid(
                    id,
                    format!("Duplicate support in unvalidated batch: {}", support),
                ));
                continue;
            }

            // Drop shares for xnet reshare transcripts
            if source_subnet_xnet_transcripts.contains(&dealing.transcript_id) {
                self.metrics.pre_sign_errors_inc("xnet_reshare_support");
                ret.push(EcdsaChangeAction::HandleInvalid(
                    id,
                    format!("Support for xnet reshare transcript: {}", support),
                ));
                continue;
            }

            // Disable the height check on target subnet side for the initial transcripts.
            // Since the transcript_id.source_height is from the source subnet, the height
            // cannot be relied upon. This also lets us process the shares for the initial
            // bootstrap with higher urgency, without deferring it.
            let msg_height = if target_subnet_xnet_transcripts.contains(&dealing.transcript_id) {
                None
            } else {
                Some(dealing.transcript_id.source_height())
            };

            match Action::action(
                block_reader,
                &requested_transcripts,
                msg_height,
                &dealing.transcript_id,
            ) {
                Action::Process(transcript_params) => {
                    if transcript_params
                        .receivers()
                        .position(support.signature.signer)
                        .is_none()
                    {
                        // The node is not in the receiver list for this transcript,
                        // support share is not expected from it
                        self.metrics.pre_sign_errors_inc("unexpected_support");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!("Support from unexpected node: {}", support),
                        ))
                    } else if !valid_dealings.contains(&dealing_key) {
                        // Support for a dealing we don't have yet, defer it
                        continue;
                    } else if self.has_node_issued_dealing_support(
                        ecdsa_pool,
                        &dealing.transcript_id,
                        &support.content.dealer_id(),
                        &support.signature.signer,
                    ) {
                        // The node already sent a valid support for this dealing
                        self.metrics.pre_sign_errors_inc("duplicate_support");
                        ret.push(EcdsaChangeAction::HandleInvalid(
                            id,
                            format!("Duplicate support: {}", support),
                        ))
                    } else {
                        let mut changes =
                            self.crypto_verify_dealing_support(&id, transcript_params, &support);
                        ret.append(&mut changes);
                    }
                }
                Action::Drop => ret.push(EcdsaChangeAction::RemoveUnvalidated(id)),
                Action::Defer => {}
            }
        }

        ret
    }

    /// Purges the entries no longer needed from the artifact pool
    fn purge_artifacts(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        block_reader: &dyn EcdsaBlockReader,
    ) -> EcdsaChangeSet {
        let requested_transcripts = resolve_transcript_refs(
            block_reader,
            "purge_artifacts",
            self.metrics.pre_sign_errors.clone(),
            &self.log,
        );

        let mut in_progress = BTreeSet::new();
        for transcript_params in requested_transcripts {
            in_progress.insert(transcript_params.transcript_id());
        }

        let mut ret = Vec::new();
        let current_height = block_reader.tip_height();
        let mut target_subnet_xnet_transcripts = BTreeSet::new();
        for transcript_params_ref in block_reader.target_subnet_xnet_transcripts() {
            target_subnet_xnet_transcripts.insert(transcript_params_ref.transcript_id);
        }

        // Unvalidated dealings.
        let mut action = ecdsa_pool
            .unvalidated()
            .signed_dealings()
            .filter(|(_, signed_dealing)| {
                self.should_purge(
                    signed_dealing.idkg_dealing(),
                    current_height,
                    &in_progress,
                    &target_subnet_xnet_transcripts,
                )
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveUnvalidated(id))
            .collect();
        ret.append(&mut action);

        // Validated dealings.
        let mut action = ecdsa_pool
            .validated()
            .signed_dealings()
            .filter(|(_, signed_dealing)| {
                self.should_purge(
                    signed_dealing.idkg_dealing(),
                    current_height,
                    &in_progress,
                    &target_subnet_xnet_transcripts,
                )
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveValidated(id))
            .collect();
        ret.append(&mut action);

        // Unvalidated dealing support.
        let mut action = ecdsa_pool
            .unvalidated()
            .dealing_support()
            .filter(|(_, support)| {
                self.should_purge(
                    &support.content.content,
                    current_height,
                    &in_progress,
                    &target_subnet_xnet_transcripts,
                )
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveUnvalidated(id))
            .collect();
        ret.append(&mut action);

        // Validated dealing support.
        let mut action = ecdsa_pool
            .validated()
            .dealing_support()
            .filter(|(_, support)| {
                self.should_purge(
                    &support.content.content,
                    current_height,
                    &in_progress,
                    &target_subnet_xnet_transcripts,
                )
            })
            .map(|(id, _)| EcdsaChangeAction::RemoveValidated(id))
            .collect();
        ret.append(&mut action);

        ret
    }

    /// Helper to create dealing
    fn crypto_create_dealing(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_loader: &dyn EcdsaTranscriptLoader,
        block_reader: &dyn EcdsaBlockReader,
        transcript_params: &IDkgTranscriptParams,
    ) -> EcdsaChangeSet {
        if let Some(changes) = self.load_dependencies(
            ecdsa_pool,
            transcript_loader,
            transcript_params,
            block_reader.tip_height(),
        ) {
            return changes;
        }

        // Create the dealing
        let idkg_dealing = match IDkgProtocol::create_dealing(&*self.crypto, transcript_params) {
            Ok(idkg_dealing) => {
                self.metrics.pre_sign_metrics_inc("dealing_created");
                idkg_dealing
            }
            Err(err) => {
                // TODO: currently, transcript creation will be retried the next time, which
                // will most likely fail again. This should be signaled up so that the bad
                // transcript params can be acted on
                warn!(
                    self.log,
                    "Failed to create dealing: transcript_id = {:?}, type = {:?}, error = {:?}",
                    transcript_params.transcript_id(),
                    transcript_op_summary(transcript_params.operation_type()),
                    err
                );
                self.metrics.pre_sign_errors_inc("create_dealing");
                return Default::default();
            }
        };

        // Corrupt the dealing if malicious testing is enabled
        #[cfg(feature = "malicious_code")]
        let idkg_dealing = self.crypto_corrupt_dealing(idkg_dealing, transcript_params);

        // Sign the dealing
        match self.crypto.sign(
            &idkg_dealing,
            self.node_id,
            transcript_params.registry_version(),
        ) {
            Ok(signature) => {
                let signed_dealing = SignedIDkgDealing {
                    signature,
                    content: idkg_dealing,
                };
                self.metrics.pre_sign_metrics_inc("dealing_sent");
                vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaSignedDealing(signed_dealing),
                )]
            }
            Err(err) => {
                warn!(
                    self.log,
                    "Failed to sign dealing: transcript_id = {:?}, type = {:?}, error = {:?}",
                    transcript_params.transcript_id(),
                    transcript_op_summary(transcript_params.operation_type()),
                    err
                );
                self.metrics.pre_sign_errors_inc("sign_dealing");
                Default::default()
            }
        }
    }

    /// Helper to verify a dealing received for a transcript we are building
    fn crypto_verify_dealing(
        &self,
        id: &EcdsaMessageId,
        transcript_params: &IDkgTranscriptParams,
        signed_dealing: &SignedIDkgDealing,
    ) -> EcdsaChangeSet {
        let dealing = signed_dealing.idkg_dealing();

        // Verify the dealer signature
        if let Err(error) = self
            .crypto
            .verify(signed_dealing, transcript_params.registry_version())
        {
            if error.is_replicated() {
                self.metrics
                    .pre_sign_errors_inc("verify_dealing_signature_permanent");
                return vec![EcdsaChangeAction::HandleInvalid(
                    id.clone(),
                    format!(
                        "Dealing signature validation(permanent error): {}, error = {:?}",
                        signed_dealing, error
                    ),
                )];
            } else {
                // Defer in case of transient errors
                debug!(
                    self.log,
                    "Dealing signature validation(transient error): {}, error = {:?}",
                    signed_dealing,
                    error
                );
                self.metrics
                    .pre_sign_errors_inc("verify_dealing_signature_transient");
                return Default::default();
            }
        }

        IDkgProtocol::verify_dealing_public(
            &*self.crypto,
            transcript_params,
            signed_dealing.dealer_id(),
            dealing,
        )
        .map_or_else(
            |error| {
                if error.is_replicated() {
                    self.metrics.pre_sign_errors_inc("verify_dealing_permanent");
                    vec![EcdsaChangeAction::HandleInvalid(
                        id.clone(),
                        format!(
                            "Dealing validation(permanent error): {}, error = {:?}",
                            signed_dealing, error
                        ),
                    )]
                } else {
                    // Defer in case of transient errors
                    debug!(
                        self.log,
                        "Dealing validation(transient error): {}, error = {:?}",
                        signed_dealing,
                        error
                    );
                    self.metrics.pre_sign_errors_inc("verify_dealing_transient");
                    Default::default()
                }
            },
            |()| {
                self.metrics.pre_sign_metrics_inc("dealing_received");
                vec![EcdsaChangeAction::MoveToValidated(id.clone())]
            },
        )
    }

    /// Helper to corrupt the crypto dealing for malicious testing
    #[cfg(feature = "malicious_code")]
    fn crypto_corrupt_dealing(
        &self,
        idkg_dealing: IDkgDealing,
        transcript_params: &IDkgTranscriptParams,
    ) -> IDkgDealing {
        if !self.malicious_flags.maliciously_corrupt_ecdsa_dealings {
            return idkg_dealing;
        }

        let mut rng = rand::thread_rng();
        let mut exclude_set = BTreeSet::new();
        exclude_set.insert(self.node_id);
        match ic_crypto_test_utils_canister_threshold_sigs::corrupt_idkg_dealing(
            &idkg_dealing,
            transcript_params,
            &exclude_set,
            &mut rng,
        ) {
            Ok(dealing) => {
                warn!(
                     every_n_seconds => 2,
                     self.log,
                    "Corrupted dealing: transcript_id = {:?}", transcript_params.transcript_id()
                );
                self.metrics.pre_sign_metrics_inc("dealing_corrupted");
                dealing
            }
            Err(err) => {
                warn!(
                    self.log,
                    "Failed to corrupt dealing: transcript_id = {:?}, type = {:?}, error = {:?}",
                    transcript_params.transcript_id(),
                    transcript_op_summary(transcript_params.operation_type()),
                    err
                );
                self.metrics.pre_sign_errors_inc("corrupt_dealing");
                idkg_dealing
            }
        }
    }

    /// Helper to issue a support share for a dealing. Assumes we are a receiver
    /// for the dealing.
    fn crypto_create_dealing_support(
        &self,
        id: &EcdsaMessageId,
        transcript_params: &IDkgTranscriptParams,
        signed_dealing: &SignedIDkgDealing,
    ) -> EcdsaChangeSet {
        let dealing = signed_dealing.idkg_dealing();
        if let Err(error) = IDkgProtocol::verify_dealing_private(
            &*self.crypto,
            transcript_params,
            signed_dealing.dealer_id(),
            dealing,
        ) {
            if error.is_replicated() {
                self.metrics
                    .pre_sign_errors_inc("verify_dealing_private_permanent");
                warn!(
                    self.log,
                    "Dealing private verification(permanent error): {}, error = {:?}",
                    dealing,
                    error
                );
                return vec![EcdsaChangeAction::HandleInvalid(
                    id.clone(),
                    format!(
                        "Dealing private verification(permanent error): {}, error = {:?}",
                        dealing, error
                    ),
                )];
            } else {
                self.metrics
                    .pre_sign_errors_inc("verify_dealing_private_transient");
                debug!(
                    self.log,
                    "Dealing private verification(transient error): {}, error = {:?}",
                    dealing,
                    error
                );
                return Default::default();
            }
        }

        // Generate the multi sig share
        self.crypto
            .sign(
                signed_dealing,
                self.node_id,
                transcript_params.registry_version(),
            )
            .map_or_else(
                |error| {
                    debug!(
                        self.log,
                        "Dealing multi sign failed: {}, error = {:?}", dealing, error
                    );
                    self.metrics
                        .pre_sign_errors_inc("dealing_support_multi_sign");
                    Default::default()
                },
                |multi_sig_share| {
                    let dealing_support = IDkgDealingSupport {
                        content: signed_dealing.clone(),
                        signature: multi_sig_share,
                    };
                    self.metrics.pre_sign_metrics_inc("dealing_support_sent");
                    vec![EcdsaChangeAction::AddToValidated(
                        EcdsaMessage::EcdsaDealingSupport(dealing_support),
                    )]
                },
            )
    }

    /// Helper to verify a support share for a dealing
    fn crypto_verify_dealing_support(
        &self,
        id: &EcdsaMessageId,
        transcript_params: &IDkgTranscriptParams,
        support: &IDkgDealingSupport,
    ) -> EcdsaChangeSet {
        self.crypto
            .verify(support, transcript_params.registry_version())
            .map_or_else(
                |error| {
                    self.metrics.pre_sign_errors_inc("verify_dealing_support");
                    vec![EcdsaChangeAction::HandleInvalid(
                        id.clone(),
                        format!(
                            "Support validation failed: {}, error = {:?}",
                            support, error
                        ),
                    )]
                },
                |_| {
                    self.metrics
                        .pre_sign_metrics_inc("dealing_support_received");
                    vec![EcdsaChangeAction::MoveToValidated(id.clone())]
                },
            )
    }

    /// Helper to load the transcripts the given transcript is dependent on.
    /// Returns true if the dependencies were loaded successfully.
    fn load_dependencies(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_loader: &dyn EcdsaTranscriptLoader,
        transcript_params: &IDkgTranscriptParams,
        height: Height,
    ) -> Option<EcdsaChangeSet> {
        match &transcript_params.operation_type() {
            IDkgTranscriptOperation::Random => None,
            IDkgTranscriptOperation::ReshareOfMasked(t) => {
                load_transcripts(ecdsa_pool, transcript_loader, &[t], height)
            }
            IDkgTranscriptOperation::ReshareOfUnmasked(t) => {
                load_transcripts(ecdsa_pool, transcript_loader, &[t], height)
            }
            IDkgTranscriptOperation::UnmaskedTimesMasked(t1, t2) => {
                load_transcripts(ecdsa_pool, transcript_loader, &[t1, t2], height)
            }
        }
    }

    /// Checks if the we have a valid dealing from the dealer for the given
    /// transcript
    fn has_dealer_issued_dealing(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_id: &IDkgTranscriptId,
        dealer_id: &NodeId,
    ) -> bool {
        ecdsa_pool
            .validated()
            .signed_dealings()
            .any(|(_, signed_dealing)| {
                let dealing = signed_dealing.idkg_dealing();
                signed_dealing.dealer_id() == *dealer_id && dealing.transcript_id == *transcript_id
            })
    }

    /// Checks if the we have a valid dealing support from the node for the
    /// given dealing
    fn has_node_issued_dealing_support(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_id: &IDkgTranscriptId,
        dealer_id: &NodeId,
        node_id: &NodeId,
    ) -> bool {
        ecdsa_pool
            .validated()
            .dealing_support()
            .any(|(_, support)| {
                support.content.dealer_id() == *dealer_id
                    && support.content.content.transcript_id == *transcript_id
                    && support.signature.signer == *node_id
            })
    }

    /// Checks if the dealing should be purged
    fn should_purge(
        &self,
        dealing: &IDkgDealing,
        current_height: Height,
        in_progress: &BTreeSet<IDkgTranscriptId>,
        target_subnet_xnet_transcripts: &BTreeSet<IDkgTranscriptId>,
    ) -> bool {
        // It is possible the ECDSA component runs and tries to purge the initial
        // dealings before the finalized tip has the next_key_transcript_creation
        // set up. Avoid this by keeping the initial dealings until the initial
        // resharing completes.
        if target_subnet_xnet_transcripts.contains(&dealing.transcript_id) {
            return false;
        }

        dealing.transcript_id.source_height() <= current_height
            && !in_progress.contains(&dealing.transcript_id)
    }
}

impl EcdsaPreSigner for EcdsaPreSignerImpl {
    fn on_state_change(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
        transcript_loader: &dyn EcdsaTranscriptLoader,
    ) -> EcdsaChangeSet {
        let block_reader = EcdsaBlockReaderImpl::new(self.consensus_block_cache.finalized_chain());
        let metrics = self.metrics.clone();

        let send_dealings = || {
            timed_call(
                "send_dealings",
                || self.send_dealings(ecdsa_pool, transcript_loader, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let validate_dealings = || {
            timed_call(
                "validate_dealings",
                || self.validate_dealings(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let send_dealing_support = || {
            timed_call(
                "send_dealing_support",
                || self.send_dealing_support(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let validate_dealing_support = || {
            timed_call(
                "validate_dealing_support",
                || self.validate_dealing_support(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };
        let purge_artifacts = || {
            timed_call(
                "purge_artifacts",
                || self.purge_artifacts(ecdsa_pool, &block_reader),
                &metrics.on_state_change_duration,
            )
        };

        let calls: [&'_ dyn Fn() -> EcdsaChangeSet; 5] = [
            &send_dealings,
            &validate_dealings,
            &send_dealing_support,
            &validate_dealing_support,
            &purge_artifacts,
        ];
        self.schedule.call_next(&calls)
    }
}

pub(crate) trait EcdsaTranscriptBuilder {
    /// Returns the specified transcript if it can be successfully
    /// built from the current entries in the ECDSA pool
    fn get_completed_transcript(&self, transcript_id: IDkgTranscriptId) -> Option<IDkgTranscript>;

    /// Returns the validated dealings for the given transcript Id from
    /// the ECDSA pool
    fn get_validated_dealings(
        &self,
        transcript_id: IDkgTranscriptId,
    ) -> BTreeMap<NodeId, IDkgDealing>;
}

pub(crate) struct EcdsaTranscriptBuilderImpl<'a> {
    requested_transcripts: Vec<IDkgTranscriptParams>,
    crypto: &'a dyn ConsensusCrypto,
    metrics: &'a EcdsaPayloadMetrics,
    ecdsa_pool: &'a dyn EcdsaPool,
    cache: RefCell<BTreeMap<IDkgTranscriptId, IDkgTranscript>>,
    log: ReplicaLogger,
}

impl<'a> EcdsaTranscriptBuilderImpl<'a> {
    pub(crate) fn new(
        block_reader: &'a dyn EcdsaBlockReader,
        crypto: &'a dyn ConsensusCrypto,
        ecdsa_pool: &'a dyn EcdsaPool,
        metrics: &'a EcdsaPayloadMetrics,
        log: ReplicaLogger,
    ) -> Self {
        let requested_transcripts = resolve_transcript_refs(
            block_reader,
            "transcript_builder",
            metrics.payload_errors.clone(),
            &log,
        );
        let cache = RefCell::new(BTreeMap::new());

        Self {
            requested_transcripts,
            crypto,
            ecdsa_pool,
            cache,
            metrics,
            log,
        }
    }

    /// Build the specified transcript from the pool.
    fn build_transcript(&self, transcript_id: IDkgTranscriptId) -> Option<IDkgTranscript> {
        // Look up the transcript params
        let transcript_params = self
            .requested_transcripts
            .iter()
            .find(|transcript_params| transcript_params.transcript_id() == transcript_id);
        let mut transcript_state = match transcript_params {
            Some(params) => TranscriptState::new(params),
            None => {
                self.metrics
                    .transcript_builder_errors_inc("missing_transcript_params");
                return None;
            }
        };

        // Step 1: Build the verified dealings from the support shares
        timed_call(
            "aggregate_dealing_support",
            || {
                for (_, signed_dealing) in self.ecdsa_pool.validated().signed_dealings() {
                    let dealing = signed_dealing.idkg_dealing();
                    if dealing.transcript_id != transcript_id {
                        continue;
                    }

                    // Collect the shares for this dealing and aggregate the shares
                    let support_shares: Vec<IDkgDealingSupport> = self
                        .ecdsa_pool
                        .validated()
                        .dealing_support()
                        .filter_map(|(_, support)| {
                            if support.content.content.transcript_id == dealing.transcript_id
                                && support.content.dealer_id() == signed_dealing.dealer_id()
                            {
                                Some(support)
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Debug for now
                    let mut content_hash = BTreeSet::new();
                    for share in &support_shares {
                        content_hash.insert(ic_crypto::crypto_hash(&share.content));
                    }
                    if content_hash.len() > 1 {
                        warn!(
                            self.log,
                            "Unexpected multi share content: support_shares = {}, content_hash = {}",
                            support_shares.len(),
                            content_hash.len()
                        );
                        self.metrics.payload_errors_inc("invalid_content_hash");
                    }

                    if let Some(multi_sig) = self.crypto_aggregate_dealing_support(
                        transcript_state.transcript_params,
                        &support_shares,
                    ) {
                        transcript_state.add_completed_dealing(signed_dealing, multi_sig);
                    }
                }
            },
            &self.metrics.transcript_builder_duration,
        );

        // Step 2: Build the transcript from the verified dealings
        timed_call(
            "create_transcript",
            || {
                self.crypto_create_transcript(
                    transcript_state.transcript_params,
                    &transcript_state.completed_dealings,
                )
            },
            &self.metrics.transcript_builder_duration,
        )
    }

    /// Helper to combine the multi sig shares for a dealing
    fn crypto_aggregate_dealing_support(
        &self,
        transcript_params: &IDkgTranscriptParams,
        support_shares: &[IDkgDealingSupport],
    ) -> Option<MultiSignature<SignedIDkgDealing>> {
        // Check if we have enough shares for aggregation
        if support_shares.len() < (transcript_params.verification_threshold().get() as usize) {
            self.metrics
                .transcript_builder_metrics_inc("insufficient_support_shares");
            return None;
        }

        let mut signatures = Vec::new();
        for support_share in support_shares {
            signatures.push(&support_share.signature);
        }

        self.crypto
            .aggregate(signatures, transcript_params.registry_version())
            .map_or_else(
                |error| {
                    warn!(
                        self.log,
                        "Failed to aggregate: transcript_id = {:?}, error = {:?}",
                        transcript_params.transcript_id(),
                        error
                    );
                    self.metrics
                        .transcript_builder_errors_inc("aggregate_dealing_support");
                    None
                },
                |multi_sig| {
                    self.metrics.transcript_builder_metrics_inc_by(
                        support_shares.len() as u64,
                        "support_aggregated",
                    );
                    self.metrics
                        .transcript_builder_metrics_inc("dealing_aggregated");
                    Some(multi_sig)
                },
            )
    }

    /// Helper to create the transcript from the verified dealings
    fn crypto_create_transcript(
        &self,
        transcript_params: &IDkgTranscriptParams,
        verified_dealings: &BTreeMap<NodeId, IDkgMultiSignedDealing>,
    ) -> Option<IDkgTranscript> {
        // Check if we have enough dealings to create transcript
        if verified_dealings.len() < (transcript_params.collection_threshold().get() as usize) {
            self.metrics
                .transcript_builder_metrics_inc("insufficient_dealings");
            return None;
        }

        IDkgProtocol::create_transcript(&*self.crypto, transcript_params, verified_dealings)
            .map_or_else(
                |error| {
                    warn!(
                        self.log,
                        "Failed to create transcript: transcript_id = {:?}, error = {:?}",
                        transcript_params.transcript_id(),
                        error
                    );
                    self.metrics
                        .transcript_builder_errors_inc("create_transcript");
                    None
                },
                |transcript| {
                    self.metrics
                        .transcript_builder_metrics_inc("transcript_created");
                    Some(transcript)
                },
            )
    }

    /// Helper to get the validated dealings.
    fn validated_dealings(&self, transcript_id: IDkgTranscriptId) -> BTreeMap<NodeId, IDkgDealing> {
        let mut ret = BTreeMap::new();
        for (_, signed_dealing) in self.ecdsa_pool.validated().signed_dealings() {
            let dealing = signed_dealing.idkg_dealing();
            if dealing.transcript_id == transcript_id {
                ret.insert(signed_dealing.dealer_id(), dealing.clone());
            }
        }
        ret
    }
}

impl<'a> EcdsaTranscriptBuilder for EcdsaTranscriptBuilderImpl<'a> {
    fn get_completed_transcript(&self, transcript_id: IDkgTranscriptId) -> Option<IDkgTranscript> {
        timed_call(
            "get_completed_transcript",
            || match self.cache.borrow_mut().entry(transcript_id) {
                Entry::Vacant(e) => self
                    .build_transcript(transcript_id)
                    .map(|transcript| e.insert(transcript).clone()),
                Entry::Occupied(e) => Some(e.get().clone()),
            },
            &self.metrics.transcript_builder_duration,
        )
    }

    fn get_validated_dealings(
        &self,
        transcript_id: IDkgTranscriptId,
    ) -> BTreeMap<NodeId, IDkgDealing> {
        timed_call(
            "get_validated_dealings",
            || self.validated_dealings(transcript_id),
            &self.metrics.transcript_builder_duration,
        )
    }
}

/// Specifies how to handle a received message
#[derive(Eq, PartialEq)]
enum Action<'a> {
    /// The message is relevant to our current state, process it
    /// immediately. The transcript params for this transcript
    /// (as specified by the finalized block) is the argument
    Process(&'a IDkgTranscriptParams),

    /// Keep it to be processed later (e.g) this is from a node
    /// ahead of us
    Defer,

    /// Don't need it
    Drop,
}

impl<'a> Action<'a> {
    /// Decides the action to take on a received message with the given
    /// height/transcriptId
    #[allow(clippy::self_named_constructors)]
    fn action(
        block_reader: &'a dyn EcdsaBlockReader,
        requested_transcripts: &'a [IDkgTranscriptParams],
        msg_height: Option<Height>,
        msg_transcript_id: &IDkgTranscriptId,
    ) -> Action<'a> {
        if let Some(height) = msg_height {
            if height > block_reader.tip_height() {
                // Message is from a node ahead of us, keep it to be
                // processed later
                return Action::Defer;
            }
        }

        for transcript_params in requested_transcripts {
            if *msg_transcript_id == transcript_params.transcript_id() {
                return Action::Process(transcript_params);
            }
        }

        // Its for a transcript that has not been requested, drop it
        Action::Drop
    }
}

/// Needed as IDKGTranscriptParams doesn't implement Debug
impl<'a> Debug for Action<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match &self {
            Self::Process(transcript_params) => {
                write!(
                    f,
                    "Action::Process(): transcript_id = {:?}",
                    transcript_params.transcript_id()
                )
            }
            Self::Defer => write!(f, "Action::Defer"),
            Self::Drop => write!(f, "Action::Drop"),
        }
    }
}

/// Helper to hold the per-transcript state during the transcript
/// building process
struct TranscriptState<'a> {
    transcript_params: &'a IDkgTranscriptParams,
    completed_dealings: BTreeMap<NodeId, IDkgMultiSignedDealing>,
}

impl<'a> TranscriptState<'a> {
    fn new(transcript_params: &'a IDkgTranscriptParams) -> Self {
        Self {
            transcript_params,
            completed_dealings: BTreeMap::new(),
        }
    }

    // Adds a completed dealing to the transcript state. The dealing
    // is stored in the IDkgMultiSignedDealing format
    fn add_completed_dealing(
        &mut self,
        signed_dealing: SignedIDkgDealing,
        multi_sig: MultiSignature<SignedIDkgDealing>,
    ) {
        let dealer_id = signed_dealing.dealer_id();
        let verified_dealing = VerifiedIdkgDealing {
            content: signed_dealing,
            signature: multi_sig,
        };
        self.completed_dealings
            .insert(dealer_id, verified_dealing.into());
    }
}

/// Resolves the IDkgTranscriptParamsRef -> IDkgTranscriptParams
fn resolve_transcript_refs(
    block_reader: &dyn EcdsaBlockReader,
    reason: &str,
    metric: IntCounterVec,
    log: &ReplicaLogger,
) -> Vec<IDkgTranscriptParams> {
    let mut ret = Vec::new();
    for transcript_params_ref in block_reader.requested_transcripts() {
        // Translate the IDkgTranscriptParamsRef -> IDkgTranscriptParams
        match transcript_params_ref.translate(block_reader) {
            Ok(transcript_params) => {
                ret.push(transcript_params);
            }
            Err(error) => {
                warn!(
                    log,
                    "Failed to translate transcript ref: reason = {}, \
                     transcript_params_ref = {:?}, tip = {:?}, error = {:?}",
                    reason,
                    transcript_params_ref,
                    block_reader.tip_height(),
                    error
                );
                metric.with_label_values(&[reason]).inc();
            }
        }
    }
    ret
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ecdsa::utils::test_utils::*;
    use ic_ecdsa_object::EcdsaObject;
    use ic_interfaces::artifact_pool::UnvalidatedArtifact;
    use ic_interfaces::ecdsa::MutableEcdsaPool;
    use ic_interfaces::time_source::TimeSource;
    use ic_test_utilities::types::ids::{NODE_1, NODE_2, NODE_3, NODE_4};
    use ic_test_utilities::with_test_replica_logger;
    use ic_test_utilities::FastForwardTimeSource;
    use ic_types::Height;

    // Tests the Action logic
    #[test]
    fn test_ecdsa_pre_signer_action() {
        let (id_1, id_2, id_3, id_4) = (
            create_transcript_id(1),
            create_transcript_id(2),
            create_transcript_id(3),
            create_transcript_id(4),
        );

        // The finalized block requests transcripts 1, 2, 3
        let nodes = [NODE_1];
        let block_reader = TestEcdsaBlockReader::for_pre_signer_test(
            Height::from(100),
            vec![
                create_transcript_param(id_1, &nodes, &nodes),
                create_transcript_param(id_2, &nodes, &nodes),
                create_transcript_param(id_3, &nodes, &nodes),
            ],
        );
        let mut requested = Vec::new();
        for transcript_params_ref in block_reader.requested_transcripts() {
            requested.push(transcript_params_ref.translate(&block_reader).unwrap());
        }

        // Message from a node ahead of us
        assert_eq!(
            Action::action(&block_reader, &requested, Some(Height::from(200)), &id_4),
            Action::Defer
        );

        // Messages for transcripts not being currently requested
        assert_eq!(
            Action::action(
                &block_reader,
                &requested,
                Some(Height::from(100)),
                &create_transcript_id(234)
            ),
            Action::Drop
        );
        assert_eq!(
            Action::action(
                &block_reader,
                &requested,
                Some(Height::from(10)),
                &create_transcript_id(234)
            ),
            Action::Drop
        );

        // Messages for transcripts currently requested
        let action = Action::action(&block_reader, &requested, Some(Height::from(100)), &id_1);
        match action {
            Action::Process(_) => {}
            _ => panic!("Unexpected action: {:?}", action),
        }

        let action = Action::action(&block_reader, &requested, Some(Height::from(10)), &id_2);
        match action {
            Action::Process(_) => {}
            _ => panic!("Unexpected action: {:?}", action),
        }
    }

    // Tests that dealings are sent for new transcripts, and requests already
    // in progress are filtered out.
    #[test]
    fn test_ecdsa_send_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let (id_1, id_2, id_3, id_4, id_5) = (
                    create_transcript_id(1),
                    create_transcript_id(2),
                    create_transcript_id(3),
                    create_transcript_id(4),
                    create_transcript_id(5),
                );

                // Set up the ECDSA pool. Pool has dealings for transcripts 1, 2, 3.
                // Only dealing for transcript 1 is issued by us.
                let dealing_1 = create_dealing(id_1, NODE_1);
                let dealing_2 = create_dealing(id_2, NODE_2);
                let dealing_3 = create_dealing(id_3, NODE_3);
                let change_set = vec![
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaSignedDealing(dealing_1)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaSignedDealing(dealing_2)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaSignedDealing(dealing_3)),
                ];
                ecdsa_pool.apply_changes(change_set);

                // Set up the transcript creation request
                // The block requests transcripts 1, 4, 5
                let t1 = create_transcript_param(id_1, &[NODE_1], &[NODE_2]);
                let t2 = create_transcript_param(id_4, &[NODE_1], &[NODE_3]);
                let t3 = create_transcript_param(id_5, &[NODE_1], &[NODE_4]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t1, t2, t3]);
                let transcript_loader: TestEcdsaTranscriptLoader = Default::default();

                // Since transcript 1 is already in progress, we should issue
                // dealings only for transcripts 4, 5
                let change_set =
                    pre_signer.send_dealings(&ecdsa_pool, &transcript_loader, &block_reader);
                assert_eq!(change_set.len(), 2);
                assert!(is_dealing_added_to_validated(&change_set, &id_4,));
                assert!(is_dealing_added_to_validated(&change_set, &id_5,));
            })
        })
    }

    // Tests that dealing is not issued if the node is in the list of dealers
    // specified by the transcript params
    #[test]
    fn test_ecdsa_non_dealers_dont_send_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (ecdsa_pool, pre_signer) = create_pre_signer_dependencies(pool_config, logger);
                let (id_1, id_2) = (create_transcript_id(1), create_transcript_id(2));

                // transcript 1 has NODE_1 as a dealer
                let t1 = create_transcript_param(id_1, &[NODE_1], &[NODE_1]);

                // Transcript 2 doesn't have NODE_1 as a dealer
                let t2 = create_transcript_param(id_2, &[NODE_2], &[NODE_2]);

                // Transcript 2 should not result in a dealing
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t1, t2]);
                let transcript_loader: TestEcdsaTranscriptLoader = Default::default();

                let change_set =
                    pre_signer.send_dealings(&ecdsa_pool, &transcript_loader, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_dealing_added_to_validated(&change_set, &id_1,));
            })
        })
    }

    // Tests that complaints are generated and added to the pool if loading transcript
    // results in complaints.
    #[test]
    fn test_ecdsa_send_dealings_with_complaints() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (ecdsa_pool, pre_signer) = create_pre_signer_dependencies(pool_config, logger);
                let (id_1, id_2, id_3) = (
                    create_transcript_id(1),
                    create_transcript_id(2),
                    create_transcript_id(3),
                );

                // Set up the transcript creation request
                // The block requests transcripts 1, 2, 3
                let t1 = create_transcript_param(id_1, &[NODE_1], &[NODE_2]);
                let t2 = create_transcript_param(id_2, &[NODE_1], &[NODE_3]);
                let t3 = create_transcript_param(id_3, &[NODE_1], &[NODE_4]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t1, t2, t3]);
                let transcript_loader =
                    TestEcdsaTranscriptLoader::new(TestTranscriptLoadStatus::Complaints);

                let change_set =
                    pre_signer.send_dealings(&ecdsa_pool, &transcript_loader, &block_reader);
                let complaints = transcript_loader.returned_complaints();
                assert_eq!(change_set.len(), complaints.len());
                assert_eq!(change_set.len(), 3);
                for complaint in complaints {
                    assert!(is_complaint_added_to_validated(
                        &change_set,
                        &complaint.content.idkg_complaint.transcript_id,
                        &NODE_1,
                        &NODE_1,
                    ));
                }
            })
        })
    }

    // Tests that received dealings are accepted/processed for eligible transcript
    // requests, and others dealings are either deferred or dropped.
    // TODO: mock crypto and test failure path
    #[test]
    fn test_ecdsa_validate_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_2, id_3, id_4) = (
                    create_transcript_id_with_height(2, Height::from(100)),
                    create_transcript_id_with_height(3, Height::from(10)),
                    create_transcript_id_with_height(4, Height::from(5)),
                );

                // Set up the transcript creation request
                // The block requests transcripts 2, 3
                let t2 = create_transcript_param(id_2, &[NODE_2], &[NODE_1]);
                let t3 = create_transcript_param(id_3, &[NODE_2], &[NODE_1]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t2, t3]);

                // Set up the ECDSA pool
                // A dealing for a transcript that is requested by finalized block (accepted)
                let dealing = create_dealing(id_2, NODE_2);
                let msg_id_2 = dealing.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is requested by finalized block (accepted)
                let dealing = create_dealing(id_3, NODE_2);
                let msg_id_3 = dealing.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is not requested by finalized block (dropped)
                let dealing = create_dealing(id_4, NODE_2);
                let msg_id_4 = dealing.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                let change_set = pre_signer.validate_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 3);
                assert!(is_moved_to_validated(&change_set, &msg_id_2));
                assert!(is_moved_to_validated(&change_set, &msg_id_3));
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_4));
            })
        })
    }

    // Tests that duplicate dealings from a dealer for the same transcript
    // are dropped.
    #[test]
    fn test_ecdsa_duplicate_dealing() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_2 = create_transcript_id_with_height(2, Height::from(100));

                // Set up the ECDSA pool
                // Validated pool has: {transcript 2, dealer = NODE_2}
                let dealing = create_dealing(id_2, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaSignedDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Unvalidated pool has: {transcript 2, dealer = NODE_2, height = 100}
                let dealing = create_dealing(id_2, NODE_2);
                let msg_id_2 = dealing.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                let t2 = create_transcript_param(id_2, &[NODE_2], &[NODE_1]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t2]);

                let change_set = pre_signer.validate_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id_2));
            })
        })
    }

    // Tests that duplicate dealings from a dealer for the same transcript
    // in the unvalidated pool are dropped.
    #[test]
    fn test_ecdsa_duplicate_dealing_in_batch() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_2 = create_transcript_id_with_height(2, Height::from(100));

                // Set up the ECDSA pool
                // Unvalidated pool has: {transcript 2, dealer = NODE_2, height = 100, internal_dealing_raw = vec[1]}
                let mut dealing = create_dealing(id_2, NODE_2);
                dealing.content.internal_dealing_raw = vec![1];
                let msg_id_2_a = dealing.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Unvalidated pool has: {transcript 2, dealer = NODE_2, height = 100, , internal_dealing_raw = vec[2]}
                let mut dealing = create_dealing(id_2, NODE_2);
                dealing.content.internal_dealing_raw = vec![2];
                let msg_id_2_b = dealing.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Unvalidated pool has: {transcript 2, dealer = NODE_3, height = 100, , internal_dealing_raw = vec[3]}
                let mut dealing = create_dealing(id_2, NODE_3);
                dealing.content.internal_dealing_raw = vec![3];
                let msg_id_3 = dealing.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                let t2 = create_transcript_param(id_2, &[NODE_2, NODE_3], &[NODE_1]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t2]);

                // msg_id_2_a, msg_id_2_a should be dropped as duplicates
                let change_set = pre_signer.validate_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 3);
                assert!(is_handle_invalid(&change_set, &msg_id_2_a));
                assert!(is_handle_invalid(&change_set, &msg_id_2_b));
                assert!(is_moved_to_validated(&change_set, &msg_id_3));
            })
        })
    }

    // Tests that dealings from a dealer that is not in the dealer list for the
    // transcript are dropped.
    #[test]
    fn test_ecdsa_unexpected_dealing() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id_2 = create_transcript_id_with_height(2, Height::from(100));

                // Unvalidated pool has: {transcript 2, dealer = NODE_2, height = 100}
                let dealing = create_dealing(id_2, NODE_2);
                let msg_id_2 = dealing.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // NODE_2 is not in the dealer list
                let t2 = create_transcript_param(id_2, &[NODE_3], &[NODE_1]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t2]);

                let change_set = pre_signer.validate_dealings(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id_2));
            })
        })
    }

    // Tests that support shares are sent to eligible dealings
    #[test]
    fn test_ecdsa_send_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let id = create_transcript_id(1);

                // We haven't sent support yet, and we are in the receiver list
                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaSignedDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);
                let t = create_transcript_param(id, &[NODE_2], &[NODE_1]);

                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);
                let change_set = pre_signer.send_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_dealing_support_added_to_validated(
                    &change_set,
                    &id,
                    &NODE_2,
                ));
                ecdsa_pool.apply_changes(change_set);

                // Since we already issued support for the dealing, it should not produce any
                // more support.
                let change_set = pre_signer.send_dealing_support(&ecdsa_pool, &block_reader);
                assert!(change_set.is_empty());
            })
        })
    }

    // Tests that support shares are not sent by nodes not in the receiver list for
    // the transcript
    #[test]
    fn test_ecdsa_non_receivers_dont_send_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let id = create_transcript_id(1);

                // We are not in the receiver list for the transcript
                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaSignedDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);
                let t = create_transcript_param(id, &[NODE_2], &[NODE_3]);

                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);
                let change_set = pre_signer.send_dealing_support(&ecdsa_pool, &block_reader);
                assert!(change_set.is_empty());
            })
        })
    }

    // Tests that support shares are not sent for transcripts we are not building
    #[test]
    fn test_ecdsa_no_support_for_missing_transcript_params() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let id = create_transcript_id(1);

                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaSignedDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![]);
                let change_set = pre_signer.send_dealing_support(&ecdsa_pool, &block_reader);
                assert!(change_set.is_empty());
            })
        })
    }

    // Tests that received support shares are accepted/processed for eligible
    // transcript requests, and others dealings are either deferred or dropped.
    #[test]
    fn test_ecdsa_validate_dealing_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_2, id_3, id_4) = (
                    create_transcript_id_with_height(2, Height::from(25)),
                    create_transcript_id_with_height(3, Height::from(10)),
                    create_transcript_id_with_height(4, Height::from(5)),
                );

                // Set up the transcript creation request
                // The block requests transcripts 2, 3
                let t2 = create_transcript_param(id_2, &[NODE_2], &[NODE_3]);
                let t3 = create_transcript_param(id_3, &[NODE_2], &[NODE_3]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t2, t3]);

                // Set up the ECDSA pool
                // A dealing for a transcript that is requested by finalized block,
                // and we already have the dealing(share accepted)
                let dealing = create_dealing(id_2, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaSignedDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                let support = create_support(id_2, NODE_2, NODE_3);
                let msg_id_2 = support.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is requested by finalized block,
                // but we don't have the dealing yet(share deferred)
                let support = create_support(id_3, NODE_2, NODE_3);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // A dealing for a transcript that is not requested by finalized block
                // (share dropped)
                let support = create_support(id_4, NODE_2, NODE_3);
                let msg_id_4 = support.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                let change_set = pre_signer.validate_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 2);
                assert!(is_moved_to_validated(&change_set, &msg_id_2));
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_4));
            })
        })
    }

    // Tests that duplicate support from a node for the same dealing
    // are dropped.
    #[test]
    fn test_ecdsa_duplicate_support_from_node() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id = create_transcript_id_with_height(1, Height::from(100));

                // Set up the ECDSA pool
                // Validated pool has: support {transcript 2, dealer = NODE_2, signer = NODE_3}
                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaSignedDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                let support = create_support(id, NODE_2, NODE_3);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaDealingSupport(support),
                )];
                ecdsa_pool.apply_changes(change_set);

                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_3}
                let support = create_support(id, NODE_2, NODE_3);
                let msg_id = support.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                let t = create_transcript_param(id, &[NODE_2], &[NODE_3]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);

                let change_set = pre_signer.validate_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id));
            })
        })
    }

    // Tests that duplicate support from a node for the same dealing
    // in the unvalidated pool are dropped.
    #[test]
    fn test_ecdsa_duplicate_support_from_node_in_batch() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id = create_transcript_id_with_height(1, Height::from(100));

                // Set up the ECDSA pool
                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_3, internal_dealing= vec[1]}
                let mut support = create_support(id, NODE_2, NODE_3);
                support.content.content.internal_dealing_raw = vec![1];
                let msg_id_1_a = support.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_3, internal_dealing = vec[2]}
                let mut support = create_support(id, NODE_2, NODE_3);
                support.content.content.internal_dealing_raw = vec![2];
                let msg_id_1_b = support.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_4}
                let dealing = create_dealing(id, NODE_2);
                let change_set = vec![EcdsaChangeAction::AddToValidated(
                    EcdsaMessage::EcdsaSignedDealing(dealing),
                )];
                ecdsa_pool.apply_changes(change_set);

                let mut support = create_support(id, NODE_2, NODE_4);
                support.content.content.internal_dealing_raw = vec![2];
                let msg_id_2 = support.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_4,
                    timestamp: time_source.get_relative_time(),
                });

                let t = create_transcript_param(id, &[NODE_2], &[NODE_3, NODE_4]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);

                let change_set = pre_signer.validate_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 3);
                assert!(is_handle_invalid(&change_set, &msg_id_1_a));
                assert!(is_handle_invalid(&change_set, &msg_id_1_b));
                assert!(is_moved_to_validated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests that support from a node that is not in the receiver list for the
    // transcript are dropped.
    #[test]
    fn test_ecdsa_unexpected_support_from_node() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let id = create_transcript_id_with_height(1, Height::from(10));

                // Unvalidated pool has: support {transcript 2, dealer = NODE_2, signer =
                // NODE_3}
                let support = create_support(id, NODE_2, NODE_3);
                let msg_id = support.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support),
                    peer_id: NODE_3,
                    timestamp: time_source.get_relative_time(),
                });

                // NODE_3 is not in the receiver list
                let t = create_transcript_param(id, &[NODE_2], &[NODE_4]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);
                let change_set = pre_signer.validate_dealing_support(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_handle_invalid(&change_set, &msg_id));
            })
        })
    }

    // Tests purging of dealings from unvalidated pool
    #[test]
    fn test_ecdsa_purge_unvalidated_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(20)),
                    create_transcript_id_with_height(2, Height::from(20)),
                    create_transcript_id_with_height(3, Height::from(200)),
                );

                // Dealing 1: height <= current_height, in_progress (not purged)
                let dealing_1 = create_dealing(id_1, NODE_2);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing_1),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Dealing 2: height <= current_height, !in_progress (purged)
                let dealing_2 = create_dealing(id_2, NODE_2);
                let msg_id_2 = dealing_2.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing_2),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Dealing 3: height > current_height (not purged)
                let dealing_3 = create_dealing(id_3, NODE_2);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaSignedDealing(dealing_3),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                let t = create_transcript_param(id_1, &[NODE_2], &[NODE_4]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);
                let change_set = pre_signer.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests purging of dealings from validated pool
    #[test]
    fn test_ecdsa_purge_validated_dealings() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(20)),
                    create_transcript_id_with_height(2, Height::from(20)),
                    create_transcript_id_with_height(3, Height::from(200)),
                );

                // Dealing 1: height <= current_height, in_progress (not purged)
                let dealing_1 = create_dealing(id_1, NODE_2);

                // Dealing 2: height <= current_height, !in_progress (purged)
                let dealing_2 = create_dealing(id_2, NODE_2);
                let msg_id_2 = dealing_2.message_hash();

                // Dealing 3: height > current_height (not purged)
                let dealing_3 = create_dealing(id_3, NODE_2);

                let change_set = vec![
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaSignedDealing(dealing_1)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaSignedDealing(dealing_2)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaSignedDealing(dealing_3)),
                ];
                ecdsa_pool.apply_changes(change_set);

                let t = create_transcript_param(id_1, &[NODE_2], &[NODE_4]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);
                let change_set = pre_signer.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_validated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests purging of dealing support from unvalidated pool
    #[test]
    fn test_ecdsa_purge_unvalidated_dealing_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let time_source = FastForwardTimeSource::new();
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(20)),
                    create_transcript_id_with_height(2, Height::from(20)),
                    create_transcript_id_with_height(3, Height::from(200)),
                );

                // Support 1: height <= current_height, in_progress (not purged)
                let support_1 = create_support(id_1, NODE_2, NODE_3);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support_1),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Dealing 2: height <= current_height, !in_progress (purged)
                let support_2 = create_support(id_2, NODE_2, NODE_3);
                let msg_id_2 = support_2.message_hash();
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support_2),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                // Dealing 3: height > current_height (not purged)
                let support_3 = create_support(id_3, NODE_2, NODE_3);
                ecdsa_pool.insert(UnvalidatedArtifact {
                    message: EcdsaMessage::EcdsaDealingSupport(support_3),
                    peer_id: NODE_2,
                    timestamp: time_source.get_relative_time(),
                });

                let t = create_transcript_param(id_1, &[NODE_2], &[NODE_4]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);
                let change_set = pre_signer.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_unvalidated(&change_set, &msg_id_2));
            })
        })
    }

    // Tests purging of dealing support from validated pool
    #[test]
    fn test_ecdsa_purge_validated_dealing_support() {
        ic_test_utilities::artifact_pool_config::with_test_pool_config(|pool_config| {
            with_test_replica_logger(|logger| {
                let (mut ecdsa_pool, pre_signer) =
                    create_pre_signer_dependencies(pool_config, logger);
                let (id_1, id_2, id_3) = (
                    create_transcript_id_with_height(1, Height::from(20)),
                    create_transcript_id_with_height(2, Height::from(20)),
                    create_transcript_id_with_height(3, Height::from(200)),
                );

                // Support 1: height <= current_height, in_progress (not purged)
                let support_1 = create_support(id_1, NODE_2, NODE_3);

                // Dealing 2: height <= current_height, !in_progress (purged)
                let support_2 = create_support(id_2, NODE_2, NODE_3);
                let msg_id_2 = support_2.message_hash();

                // Dealing 3: height > current_height (not purged)
                let support_3 = create_support(id_3, NODE_2, NODE_3);

                let change_set = vec![
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealingSupport(support_1)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealingSupport(support_2)),
                    EcdsaChangeAction::AddToValidated(EcdsaMessage::EcdsaDealingSupport(support_3)),
                ];
                ecdsa_pool.apply_changes(change_set);

                let t = create_transcript_param(id_1, &[NODE_2], &[NODE_4]);
                let block_reader =
                    TestEcdsaBlockReader::for_pre_signer_test(Height::from(100), vec![t]);
                let change_set = pre_signer.purge_artifacts(&ecdsa_pool, &block_reader);
                assert_eq!(change_set.len(), 1);
                assert!(is_removed_from_validated(&change_set, &msg_id_2));
            })
        })
    }
}
