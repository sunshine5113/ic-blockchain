//! ECDSA related public interfaces.

use crate::artifact_pool::UnvalidatedArtifact;
use ic_types::artifact::{EcdsaMessageAttribute, EcdsaMessageId, PriorityFn};
use ic_types::consensus::ecdsa::{EcdsaComplaint, EcdsaMessage, EcdsaOpening, EcdsaSigShare};
use ic_types::crypto::canister_threshold_sig::idkg::{IDkgDealingSupport, SignedIDkgDealing};

// TODO: purge/remove from validated
#[derive(Debug)]
pub enum EcdsaChangeAction {
    AddToValidated(EcdsaMessage),
    MoveToValidated(EcdsaMessageId),
    RemoveValidated(EcdsaMessageId),
    RemoveUnvalidated(EcdsaMessageId),
    HandleInvalid(EcdsaMessageId, String),
}

pub type EcdsaChangeSet = Vec<EcdsaChangeAction>;

/// The validated/unvalidated parts of the artifact pool.
pub trait EcdsaPoolSection: Send + Sync {
    /// Checks if the artifact present in the pool.
    fn contains(&self, msg_id: &EcdsaMessageId) -> bool;

    /// Looks up an artifact by the Id.
    fn get(&self, msg_id: &EcdsaMessageId) -> Option<EcdsaMessage>;

    /// Iterator for signed dealing objects.
    fn signed_dealings(&self)
        -> Box<dyn Iterator<Item = (EcdsaMessageId, SignedIDkgDealing)> + '_>;

    /// Iterator for dealing support objects.
    fn dealing_support(
        &self,
    ) -> Box<dyn Iterator<Item = (EcdsaMessageId, IDkgDealingSupport)> + '_>;

    /// Iterator for signature share objects.
    fn signature_shares(&self) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaSigShare)> + '_>;

    /// Iterator for complaint objects.
    fn complaints(&self) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaComplaint)> + '_>;

    /// Iterator for opening objects.
    fn openings(&self) -> Box<dyn Iterator<Item = (EcdsaMessageId, EcdsaOpening)> + '_>;
}

/// The mutable interface for validated/unvalidated parts of the artifact pool.
pub trait MutableEcdsaPoolSection: Send + Sync {
    /// Adds the message to the pool.
    fn insert(&mut self, message: EcdsaMessage);

    /// Looks up and removes the specified message from the pool.
    /// Returns true if the message was found.
    fn remove(&mut self, id: &EcdsaMessageId) -> bool;

    /// Get the immutable handle.
    fn as_pool_section(&self) -> &dyn EcdsaPoolSection;
}

/// Artifact pool for the ECDSA messages (query interface)
pub trait EcdsaPool: Send + Sync {
    /// Return a reference to the validated PoolSection.
    fn validated(&self) -> &dyn EcdsaPoolSection;

    /// Return a reference to the unvalidated PoolSection.
    fn unvalidated(&self) -> &dyn EcdsaPoolSection;
}

/// Artifact pool for the ECDSA messages (update interface)
pub trait MutableEcdsaPool: EcdsaPool {
    /// Adds the entry to the unvalidated section of the artifact pool.
    fn insert(&mut self, msg: UnvalidatedArtifact<EcdsaMessage>);

    /// Mutates the artifact pool by applying the change set.
    fn apply_changes(&mut self, change_set: EcdsaChangeSet);
}

/// Checks and processes the changes (if any)
pub trait Ecdsa: Send {
    fn on_state_change(&self, ecdsa_pool: &dyn EcdsaPool) -> EcdsaChangeSet;
}

pub trait EcdsaGossip: Send + Sync {
    fn get_priority_function(
        &self,
        ecdsa_pool: &dyn EcdsaPool,
    ) -> PriorityFn<EcdsaMessageId, EcdsaMessageAttribute>;
}
