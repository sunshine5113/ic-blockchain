mod queue;
#[cfg(test)]
mod tests;

use crate::{InputQueueType, NextInputQueue, StateError};
use ic_interfaces::messages::CanisterInputMessage;
use ic_protobuf::{
    proxy::{try_from_option_field, ProxyDecodeError},
    state::queues::{v1 as pb_queues, v1::canister_queues::NextInputQueue as ProtoNextInputQueue},
    types::v1 as pb_types,
};
use ic_types::{
    messages::{Ingress, Request, RequestOrResponse, Response, MAX_RESPONSE_COUNT_BYTES},
    xnet::{QueueId, SessionId},
    CanisterId, CountBytes, Cycles, QueueIndex,
};
use queue::{IngressQueue, InputQueue, OutputQueue};
use std::{
    collections::{BTreeMap, VecDeque},
    convert::{From, TryFrom},
    ops::{AddAssign, SubAssign},
    sync::Arc,
};

pub const DEFAULT_QUEUE_CAPACITY: usize = 500;

/// "None" queue index used internally by Message Routing for reject responses
/// generated e.g. when a request cannot be inducted due to a full input queue
/// (and enqueuing the response into the output queue might also fail).
pub const QUEUE_INDEX_NONE: QueueIndex = QueueIndex::new(std::u64::MAX);

/// Wrapper around the induction pool (ingress and input queues); a priority
/// queue used for round-robin scheduling of senders when consuming input
/// messages; and output queues.
///
/// Responsible for queue lifetime management, fair scheduling of inputs across
/// sender canisters and queue backpressure.
///
/// Encapsulates the `InductionPool` component described in the spec. The reason
/// for bundling together the induction pool and output queues is to reliably
/// implement backpressure via queue reservations for response messages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CanisterQueues {
    /// Queue of ingress (user) messages.
    ingress_queue: IngressQueue,

    /// Per remote canister input and output queues.
    canister_queues: BTreeMap<CanisterId, (InputQueue, OutputQueue)>,

    /// FIFO queue of Local Subnet sender canister IDs ensuring round-robin
    /// consumption of input messages. Only senders with non-empty queues
    /// are scheduled.
    local_subnet_input_schedule: VecDeque<CanisterId>,

    /// FIFO queue of Remote Subnet sender canister IDs ensuring round-robin
    /// consumption of input messages. Only senders with non-empty queues
    /// are scheduled.
    remote_subnet_input_schedule: VecDeque<CanisterId>,

    /// Running `input_queues` stats.
    input_queues_stats: InputQueuesStats,

    /// Running `output_queues` stats.
    output_queues_stats: OutputQueuesStats,

    /// Running memory usage stats, across input and output queues.
    memory_usage_stats: MemoryUsageStats,

    /// Round-robin across ingress and cross-net input queues for pop_input().
    next_input_queue: NextInputQueue,
}

/// Circular iterator that consumes output queue messages: loops over output
/// queues, popping one message at a time from each in a round robin fashion.
/// All messages that have not been explicitly popped will remain in the state.
///
/// Additional operations compared to a standard iterator:
///  * peeking (returning a reference to the next message without consuming it);
///    and
///  * excluding whole queues from iteration while retaining their messages
///    (e.g. in order to efficiently implement per destination limits).
#[derive(Debug)]
pub struct CanisterOutputQueuesIterator<'a> {
    /// ID of the canister that owns the output queues being iterated.
    owner: CanisterId,

    /// Priority queue of non-empty output queues. The next message to be popped
    /// / peeked is the one at the head of the first queue.
    queues: VecDeque<(&'a CanisterId, &'a mut OutputQueue)>,

    /// Number of messages that can be popped before the iterator finishes.
    size: usize,

    /// The canister's memory usage stats, to be updated as messages are popped.
    memory_stats: &'a mut MemoryUsageStats,

    /// Canister output queue stats, to be updated as messages are popped.
    queue_stats: &'a mut OutputQueuesStats,
}

impl<'a> CanisterOutputQueuesIterator<'a> {
    fn new(
        owner: CanisterId,
        queues: &'a mut BTreeMap<CanisterId, (InputQueue, OutputQueue)>,
        memory_stats: &'a mut MemoryUsageStats,
        queue_stats: &'a mut OutputQueuesStats,
    ) -> Self {
        let queues: VecDeque<_> = queues
            .iter_mut()
            .filter(|(_, (_, queue))| queue.num_messages() > 0)
            .map(|(canister, (_, queue))| (canister, queue))
            .collect();
        let size = Self::compute_size(&queues);

        CanisterOutputQueuesIterator {
            owner,
            queues,
            size,
            memory_stats,
            queue_stats,
        }
    }

    /// Returns a reference to the message that `pop` / `next` would return.
    pub fn peek(&self) -> Option<(QueueId, QueueIndex, &RequestOrResponse)> {
        if let Some((receiver, queue)) = self.queues.front() {
            let (queue_index, msg) = queue.peek().expect("Empty queue in iterator");
            let queue_id = QueueId {
                src_canister: self.owner,
                dst_canister: **receiver,
                session_id: SessionId::new(0),
            };
            return Some((queue_id, queue_index, msg));
        }
        None
    }

    /// Pops a message from the next queue. If this was not the last message in
    /// that queue, the queue is moved to the back of the iteration order.
    pub fn pop(&mut self) -> Option<(QueueId, QueueIndex, RequestOrResponse)> {
        if let Some((receiver, queue)) = self.queues.pop_front() {
            let (queue_index, msg) = queue.pop().expect("Empty queue in iterator");
            let queue_id = QueueId {
                src_canister: self.owner,
                dst_canister: *receiver,
                session_id: SessionId::new(0),
            };

            if queue.num_messages() > 0 {
                self.queues.push_back((receiver, queue));
            }

            *self.memory_stats -= MemoryUsageStats::stats_delta(QueueOp::Pop, &msg);
            *self.queue_stats -= OutputQueuesStats::stats_delta(&msg);
            self.size -= 1;
            debug_assert_eq!(Self::compute_size(&self.queues), self.size);

            return Some((queue_id, queue_index, msg));
        }
        None
    }

    /// Permanently excludes from iteration the next queue (i.e. all messages
    /// with the same sender and receiver as the next message). The mesages are
    /// retained in the output queue.
    ///
    /// Returns the number of messages left in the excluded queue.
    pub fn exclude_queue(&mut self) -> usize {
        let ignored = self
            .queues
            .pop_front()
            .map(|(_, q)| q.num_messages())
            .unwrap_or_default();

        self.size -= ignored;
        debug_assert_eq!(Self::compute_size(&self.queues), self.size);

        ignored
    }

    /// Checks if the iterator has finished.
    pub fn is_empty(&self) -> bool {
        self.queues.is_empty()
    }

    /// Computes the number of messages left in `queues`.
    ///
    /// Time complexity: O(N).
    fn compute_size(queues: &VecDeque<(&'a CanisterId, &'a mut OutputQueue)>) -> usize {
        queues.iter().map(|(_, q)| q.num_messages()).sum()
    }
}

impl Iterator for CanisterOutputQueuesIterator<'_> {
    type Item = (QueueId, QueueIndex, RequestOrResponse);

    /// Alias for `pop`.
    fn next(&mut self) -> Option<Self::Item> {
        self.pop()
    }

    /// Returns the exact number of messages left in the iterator.
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.size, Some(self.size))
    }
}

impl CanisterQueues {
    /// Pushes an ingress message into the induction pool.
    pub fn push_ingress(&mut self, msg: Ingress) {
        self.ingress_queue.push(msg)
    }

    /// Pops the next ingress message from `ingress_queue`.
    fn pop_ingress(&mut self) -> Option<Arc<Ingress>> {
        self.ingress_queue.pop()
    }

    /// For each output queue, invokes `f` on every message until `f` returns
    /// `Err`; then moves on to the next output queue.
    ///
    /// All messages that `f` returned `Ok` for, are popped. Messages that `f`
    /// returned `Err` for and all those following them in the respective output
    /// queue are retained.
    pub(crate) fn output_queues_for_each<F>(&mut self, mut f: F)
    where
        F: FnMut(&CanisterId, &RequestOrResponse) -> Result<(), ()>,
    {
        for (canister_id, (_, queue)) in self.canister_queues.iter_mut() {
            while let Some((_, msg)) = queue.peek() {
                match f(canister_id, msg) {
                    Err(_) => break,
                    Ok(_) => {
                        let msg = queue
                            .pop()
                            .expect("peek() returned a message, pop() should not fail")
                            .1;
                        let oq_stats_delta = OutputQueuesStats::stats_delta(&msg);
                        self.output_queues_stats -= oq_stats_delta;
                        self.memory_usage_stats -=
                            MemoryUsageStats::stats_delta(QueueOp::Pop, &msg);
                    }
                }
            }
        }
        debug_assert!(self.stats_ok());
    }

    /// Returns an iterator that loops over output queues, popping one message
    /// at a time from each in a round robin fashion. The iterator consumes all
    /// popped messages.
    pub(crate) fn output_into_iter(&mut self, owner: CanisterId) -> CanisterOutputQueuesIterator {
        CanisterOutputQueuesIterator::new(
            owner,
            &mut self.canister_queues,
            &mut self.memory_usage_stats,
            &mut self.output_queues_stats,
        )
    }

    /// See `IngressQueue::filter_messages()` for documentation.
    pub fn filter_ingress_messages<F>(&mut self, filter: F)
    where
        F: FnMut(&Arc<Ingress>) -> bool,
    {
        self.ingress_queue.filter_messages(filter);
    }

    /// Pushes a canister-to-canister message into the induction pool.
    ///
    /// If the message is a `Request` this will also reserve a slot in the
    /// corresponding output queue for the eventual response.
    ///
    /// If the message is a `Response` the protocol will have already reserved
    /// space for it, so the push cannot fail due to the input queue being
    /// full.
    ///
    /// # Errors
    ///
    /// If pushing fails, returns the provided message along with a
    /// `StateError`:
    ///
    ///  * `QueueFull` if pushing a `Request` and the corresponding input or
    ///    output queues are full.
    ///
    ///  * `QueueFull` if pushing a `Response` and the receiving canister is not
    ///  expecting one.
    pub(super) fn push_input(
        &mut self,
        index: QueueIndex,
        msg: RequestOrResponse,
        input_queue_type: InputQueueType,
    ) -> Result<(), (StateError, RequestOrResponse)> {
        let sender = msg.sender();
        let input_queue = match msg {
            RequestOrResponse::Request(_) => {
                let (input_queue, output_queue) = self.get_or_insert_queues(&sender);
                if let Err(e) = input_queue.check_has_slot() {
                    return Err((e, msg));
                }
                // Safe to already (attempt to) reserve an output slot here, as the `push()`
                // below is guaranteed to succeed due to the check above.
                if let Err(e) = output_queue.reserve_slot() {
                    return Err((e, msg));
                }
                input_queue
            }
            RequestOrResponse::Response(_) => match self.canister_queues.get_mut(&sender) {
                Some((queue, _)) => queue,
                None => return Err((StateError::QueueFull { capacity: 0 }, msg)),
            },
        };
        let iq_stats_delta = InputQueuesStats::stats_delta(QueueOp::Push, &msg);
        let mu_stats_delta = MemoryUsageStats::stats_delta(QueueOp::Push, &msg);

        input_queue.push(index, msg)?;

        // Add sender canister ID to the input schedule queue if it isn't already there.
        // Sender was not scheduled iff its input queue was empty before the push (i.e.
        // queue size is 1 after the push).
        if input_queue.num_messages() == 1 {
            match input_queue_type {
                InputQueueType::LocalSubnet => self.local_subnet_input_schedule.push_back(sender),
                InputQueueType::RemoteSubnet => self.remote_subnet_input_schedule.push_back(sender),
            }
        }

        self.input_queues_stats += iq_stats_delta;
        self.memory_usage_stats += mu_stats_delta;
        debug_assert!(self.stats_ok());

        Ok(())
    }

    /// Pops the next canister-to-canister message from `input_queues`.
    ///
    /// Note: We pop senders from the head of `input_schedule` and insert them
    /// to the back, which allows us to handle messages from different
    /// originators in a round-robin fashion.
    fn pop_canister_input(&mut self, input_queue: InputQueueType) -> Option<RequestOrResponse> {
        let input_schedule = match input_queue {
            InputQueueType::LocalSubnet => &mut self.local_subnet_input_schedule,
            InputQueueType::RemoteSubnet => &mut self.remote_subnet_input_schedule,
        };
        if let Some(sender) = input_schedule.pop_front() {
            // Get the message queue of this canister.
            let input_queue = &mut self.canister_queues.get_mut(&sender).unwrap().0;
            let msg = input_queue.pop().unwrap();
            // If the queue still isn't empty, re-add sender canister ID to the end of the
            // input schedule queue.
            if input_queue.num_messages() != 0 {
                input_schedule.push_back(sender);
            }

            self.input_queues_stats -= InputQueuesStats::stats_delta(QueueOp::Pop, &msg);
            self.memory_usage_stats -= MemoryUsageStats::stats_delta(QueueOp::Pop, &msg);
            debug_assert!(self.stats_ok());

            return Some(msg);
        }

        None
    }

    /// Returns `true` if `ingress_queue` or at least one of the `input_queues`
    /// is not empty; `false` otherwise.
    pub fn has_input(&self) -> bool {
        !self.ingress_queue.is_empty() || self.input_queues_stats.message_count > 0
    }

    /// Returns `true` if at least one output queue is not empty; false
    /// otherwise.
    pub fn has_output(&self) -> bool {
        self.output_queues_stats.message_count > 0
    }

    /// Extracts the next ingress, priority, or normal message (round-robin).
    ///
    /// We define three buckets of queues: messages from canisters on the same
    /// subnet (local subnet), ingress, and messages from canisters on other
    /// subnets (remote subnet).
    ///
    /// Each time this function is called, we round robin between these three
    /// buckets. We also round robin between the queues in the local subnet and
    /// remote subnet buckets when we pop messages from those buckets.
    pub(crate) fn pop_input(&mut self) -> Option<CanisterInputMessage> {
        // Try all 3 input: Ingress, Local, and Remote subnets
        for _ in 0..3 {
            let cur_input_queue = self.next_input_queue;
            // Switch to the next input queue
            self.next_input_queue = match self.next_input_queue {
                NextInputQueue::LocalSubnet => NextInputQueue::Ingress,
                NextInputQueue::Ingress => NextInputQueue::RemoteSubnet,
                NextInputQueue::RemoteSubnet => NextInputQueue::LocalSubnet,
            };

            let next_input = match cur_input_queue {
                NextInputQueue::Ingress => self.pop_ingress().map(CanisterInputMessage::Ingress),

                NextInputQueue::RemoteSubnet => self
                    .pop_canister_input(InputQueueType::RemoteSubnet)
                    .map(|msg| match msg {
                        RequestOrResponse::Request(msg) => CanisterInputMessage::Request(msg),
                        RequestOrResponse::Response(msg) => CanisterInputMessage::Response(msg),
                    }),

                NextInputQueue::LocalSubnet => self
                    .pop_canister_input(InputQueueType::LocalSubnet)
                    .map(|msg| match msg {
                        RequestOrResponse::Request(msg) => CanisterInputMessage::Request(msg),
                        RequestOrResponse::Response(msg) => CanisterInputMessage::Response(msg),
                    }),
            };

            if next_input.is_some() {
                return next_input;
            }
        }

        None
    }

    /// Pushes a `Request` type message into the relevant output queue. Also
    /// reserves a slot for the eventual response on the matching input queue.
    ///
    /// # Errors
    ///
    /// Returns a `QueueFull` error along with the provided message if either
    /// the output queue or the matching input queue is full.
    pub fn push_output_request(
        &mut self,
        msg: Arc<Request>,
    ) -> Result<(), (StateError, Arc<Request>)> {
        let (input_queue, output_queue) = self.get_or_insert_queues(&msg.receiver);

        if let Err(e) = output_queue.check_has_slot() {
            return Err((e, msg));
        }
        if let Err(e) = input_queue.reserve_slot() {
            return Err((e, msg));
        }

        let mu_stats_delta = MemoryUsageStats::request_stats_delta(QueueOp::Push, &msg);
        let oq_stats_delta =
            OutputQueuesStats::stats_delta(&RequestOrResponse::Request(msg.clone()));

        output_queue
            .push_request(msg)
            .expect("cannot fail due to checks above");

        self.input_queues_stats.reserved_slots += 1;
        self.output_queues_stats += oq_stats_delta;
        self.memory_usage_stats += mu_stats_delta;
        debug_assert!(self.stats_ok());

        Ok(())
    }

    /// Returns the number of output requests that can be pushed to each
    /// canister before either the respective input or output queue is full.
    pub fn available_output_request_slots(&self) -> BTreeMap<CanisterId, usize> {
        // When pushing a request we need to reserve a slot on the input
        // queue for the eventual reply. So we are limited by the amount of
        // space in both the output and input queues.
        self.canister_queues
            .iter()
            .map(|(canister, (input_queue, output_queue))| {
                (
                    *canister,
                    input_queue
                        .available_slots()
                        .min(output_queue.available_slots()),
                )
            })
            .collect()
    }

    /// Pushes a `Response` type message into the relevant output queue. The
    /// protocol should have already reserved a slot, so this cannot fail.
    ///
    /// # Panics
    ///
    /// Panics if the queue does not already exist or there is no reserved slot
    /// to push the `Response` into.
    pub fn push_output_response(&mut self, msg: Arc<Response>) {
        let mu_stats_delta = MemoryUsageStats::response_stats_delta(QueueOp::Push, &msg);
        let oq_stats_delta =
            OutputQueuesStats::stats_delta(&RequestOrResponse::Response(msg.clone()));

        // As long as we are not garbage collecting output queues, we are guaranteed
        // that an output queue should exist for pushing responses because one would
        // have been created when the request (that triggered this response) was
        // inducted into the induction pool.
        self.canister_queues
            .get_mut(&msg.originator)
            .expect("pushing response into inexistent output queue")
            .1
            .push_response(msg);

        self.memory_usage_stats += mu_stats_delta;
        self.output_queues_stats += oq_stats_delta;
        debug_assert!(self.stats_ok());
    }

    /// Returns a reference to the message at the head of the respective output
    /// queue, if any.
    pub(super) fn peek_output(&self, canister_id: &CanisterId) -> Option<&RequestOrResponse> {
        Some(self.canister_queues.get(canister_id)?.1.peek()?.1)
    }

    /// Tries to induct a message from the output queue to `own_canister_id`
    /// into the input queue from `own_canister_id`. Returns `Err(())` if there
    /// was no message to induct or the input queue was full.
    pub(super) fn induct_message_to_self(&mut self, own_canister_id: CanisterId) -> Result<(), ()> {
        let msg = self
            .canister_queues
            .get(&own_canister_id)
            .and_then(|(_, output_queue)| output_queue.peek())
            .ok_or(())?
            .1
            .clone();

        self.push_input(QUEUE_INDEX_NONE, msg, InputQueueType::LocalSubnet)
            .map_err(|_| ())?;

        let msg = self
            .canister_queues
            .get_mut(&own_canister_id)
            .expect("Output queue existed above so should not fail.")
            .1
            .pop()
            .expect("Message peeked above so pop should not fail.")
            .1;
        let oq_stats_delta = OutputQueuesStats::stats_delta(&msg);
        self.output_queues_stats -= oq_stats_delta;
        self.memory_usage_stats -= MemoryUsageStats::stats_delta(QueueOp::Pop, &msg);

        Ok(())
    }

    /// Returns the number of enqueued ingress messages.
    pub fn ingress_queue_message_count(&self) -> usize {
        self.ingress_queue.size()
    }

    /// Returns the total byte size of enqueued ingress messages.
    pub fn ingress_queue_size_bytes(&self) -> usize {
        self.ingress_queue.count_bytes()
    }

    /// Returns the number of canister messages enqueued in input queues.
    pub fn input_queues_message_count(&self) -> usize {
        self.input_queues_stats.message_count
    }

    /// Returns the number of reservations across all input queues.
    pub fn input_queues_reservation_count(&self) -> usize {
        self.input_queues_stats.reserved_slots as usize
    }

    /// Returns total amount of cycles included in input queues.
    pub fn input_queue_cycles(&self) -> Cycles {
        self.input_queues_stats.cycles
    }

    /// Returns the number of canister messages enqueued in output queues.
    pub fn output_queues_message_count(&self) -> usize {
        self.output_queues_stats.message_count
    }

    /// Returns total amount of cycles included in the output queues.
    pub fn output_queue_cycles(&self) -> Cycles {
        self.output_queues_stats.cycles
    }

    /// Returns the total byte size of canister input queues (queues +
    /// messages).
    pub fn input_queues_size_bytes(&self) -> usize {
        self.input_queues_stats.size_bytes
    }

    pub fn input_queues_response_count(&self) -> usize {
        self.input_queues_stats.response_count
    }

    /// Returns input queues stats.
    pub fn input_queues_stats(&self) -> &InputQueuesStats {
        &self.input_queues_stats
    }

    /// Returns the memory usage of this `CanisterQueues`.
    pub fn memory_usage(&self) -> usize {
        self.memory_usage_stats.memory_usage()
    }

    /// Returns the total byte size of canister responses across input and
    /// output queues.
    pub fn responses_size_bytes(&self) -> usize {
        self.memory_usage_stats.responses_size_bytes
    }

    /// Returns the total reserved slots across input and output queues.
    pub fn reserved_slots(&self) -> usize {
        self.memory_usage_stats.reserved_slots as usize
    }

    /// Returns the sum total of bytes above `MAX_RESPONSE_COUNT_BYTES` per
    /// oversized request.
    pub fn oversized_requests_extra_bytes(&self) -> usize {
        self.memory_usage_stats.oversized_requests_extra_bytes as usize
    }

    /// Sets the (transient) size in bytes of responses routed from
    /// `output_queues` into streams and not yet garbage collected.
    pub(super) fn set_stream_responses_size_bytes(&mut self, size_bytes: usize) {
        self.memory_usage_stats
            .transient_stream_responses_size_bytes = size_bytes;
    }

    /// Returns the byte size of responses already routed to streams as set by
    /// the last call to `set_stream_responses_size_bytes()`.
    pub fn stream_responses_size_bytes(&self) -> usize {
        self.memory_usage_stats
            .transient_stream_responses_size_bytes
    }

    /// Returns an existing a matching pair of input and output queues from/to
    /// the given canister; or creates a pair of empty queues, if non-existent.
    fn get_or_insert_queues(
        &mut self,
        canister_id: &CanisterId,
    ) -> (&mut InputQueue, &mut OutputQueue) {
        let mut queue_bytes = 0;
        let (input_queue, output_queue) =
            self.canister_queues.entry(*canister_id).or_insert_with(|| {
                let input_queue = InputQueue::new(DEFAULT_QUEUE_CAPACITY);
                let output_queue = OutputQueue::new(DEFAULT_QUEUE_CAPACITY);
                queue_bytes = input_queue.calculate_size_bytes();
                (input_queue, output_queue)
            });
        self.input_queues_stats.size_bytes += queue_bytes;
        (input_queue, output_queue)
    }

    /// Helper function to concisely validate stats adjustments in debug builds,
    /// by writing `debug_assert!(self.stats_ok())`.
    fn stats_ok(&self) -> bool {
        debug_assert_eq!(
            Self::calculate_input_queues_stats(&self.canister_queues),
            self.input_queues_stats
        );
        debug_assert_eq!(
            Self::calculate_memory_usage_stats(&self.canister_queues),
            self.memory_usage_stats
        );
        true
    }

    /// Computes input queues stats from scratch. Used when deserializing and
    /// in `debug_assert!()` checks.
    ///
    /// Time complexity: O(num_messages).
    fn calculate_input_queues_stats(
        canister_queues: &BTreeMap<CanisterId, (InputQueue, OutputQueue)>,
    ) -> InputQueuesStats {
        let mut stats = InputQueuesStats::default();
        let response_count = |msg: &RequestOrResponse| match *msg {
            RequestOrResponse::Request(_) => 0,
            RequestOrResponse::Response(_) => 1,
        };
        for (q, _) in canister_queues.values() {
            stats.message_count += q.num_messages();
            stats.response_count += q.calculate_stat_sum(response_count);
            stats.reserved_slots += q.reserved_slots() as isize;
            stats.size_bytes += q.calculate_size_bytes();
            stats.cycles += q.cycles_in_queue();
        }
        stats
    }

    /// Computes output queues stats from scratch. Used when deserializing and
    /// in `debug_assert!()` checks.
    ///
    /// Time complexity: O(num_messages).
    fn calculate_output_queues_stats(
        canister_queues: &BTreeMap<CanisterId, (InputQueue, OutputQueue)>,
    ) -> OutputQueuesStats {
        let mut stats = OutputQueuesStats::default();
        for (_, q) in canister_queues.values() {
            stats.message_count += q.num_messages();
            stats.cycles += q.cycles_in_queue();
        }
        stats
    }

    /// Computes memory usage stats from scratch. Used when deserializing and in
    /// `debug_assert!()` checks.
    ///
    /// Time complexity: O(num_messages).
    fn calculate_memory_usage_stats(
        canister_queues: &BTreeMap<CanisterId, (InputQueue, OutputQueue)>,
    ) -> MemoryUsageStats {
        // Actual byte size for responses, 0 for requests.
        let response_size_bytes = |msg: &RequestOrResponse| match *msg {
            RequestOrResponse::Request(_) => 0,
            RequestOrResponse::Response(_) => msg.count_bytes(),
        };
        // `max(0, msg.count_bytes() - MAX_RESPONSE_COUNT_BYTES)` for requests, 0 for
        // responses.
        let request_overhead_bytes = |msg: &RequestOrResponse| match *msg {
            RequestOrResponse::Request(_) => {
                msg.count_bytes().saturating_sub(MAX_RESPONSE_COUNT_BYTES)
            }
            RequestOrResponse::Response(_) => 0,
        };

        let mut stats = MemoryUsageStats::default();
        for (iq, oq) in canister_queues.values() {
            stats.responses_size_bytes += iq.calculate_stat_sum(response_size_bytes);
            stats.reserved_slots += iq.reserved_slots() as i64;
            stats.oversized_requests_extra_bytes += iq.calculate_stat_sum(request_overhead_bytes);

            stats.responses_size_bytes += oq.calculate_stat_sum(response_size_bytes);
            stats.reserved_slots += oq.reserved_slots() as i64;
            stats.oversized_requests_extra_bytes += oq.calculate_stat_sum(request_overhead_bytes)
        }
        stats
    }
}

impl From<&CanisterQueues> for pb_queues::CanisterQueues {
    fn from(item: &CanisterQueues) -> Self {
        Self {
            ingress_queue: (&item.ingress_queue).into(),
            input_queues: item
                .canister_queues
                .iter()
                .map(|(canid, (input_queue, _))| pb_queues::QueueEntry {
                    canister_id: Some(pb_types::CanisterId::from(*canid)),
                    queue: Some(input_queue.into()),
                })
                .collect(),
            output_queues: item
                .canister_queues
                .iter()
                .map(|(canid, (_, output_queue))| pb_queues::QueueEntry {
                    canister_id: Some(pb_types::CanisterId::from(*canid)),
                    queue: Some(output_queue.into()),
                })
                .collect(),
            // TODO: input_schedule is deprecated and should be removed next release
            input_schedule: [].into(),
            next_input_queue: match item.next_input_queue {
                NextInputQueue::LocalSubnet => ProtoNextInputQueue::LocalSubnet,
                NextInputQueue::Ingress => ProtoNextInputQueue::Ingress,
                NextInputQueue::RemoteSubnet => ProtoNextInputQueue::RemoteSubnet,
            } as i32,
            local_subnet_input_schedule: item
                .local_subnet_input_schedule
                .iter()
                .map(|canid| pb_types::CanisterId::from(*canid))
                .collect(),
            remote_subnet_input_schedule: item
                .remote_subnet_input_schedule
                .iter()
                .map(|canid| pb_types::CanisterId::from(*canid))
                .collect(),
        }
    }
}

impl TryFrom<pb_queues::CanisterQueues> for CanisterQueues {
    type Error = ProxyDecodeError;
    fn try_from(item: pb_queues::CanisterQueues) -> Result<Self, Self::Error> {
        if item.input_queues.len() != item.output_queues.len() {
            return Err(ProxyDecodeError::Other(format!(
                "CanisterQueues: Mismatched input ({}) and output ({}) queue lengths",
                item.input_queues.len(),
                item.output_queues.len()
            )));
        }
        let mut canister_queues = BTreeMap::new();
        for (ie, oe) in item
            .input_queues
            .into_iter()
            .zip(item.output_queues.into_iter())
        {
            if ie.canister_id != oe.canister_id {
                return Err(ProxyDecodeError::Other(format!(
                    "Mismatched input {:?} and output {:?} queue entries",
                    ie.canister_id, oe.canister_id
                )));
            }

            let can_id = try_from_option_field(ie.canister_id, "CanisterQueues::input_queues::K")?;
            let iq = try_from_option_field(ie.queue, "CanisterQueues::input_queues::V")?;
            let oq = try_from_option_field(oe.queue, "CanisterQueues::output_queues::V")?;
            canister_queues.insert(can_id, (iq, oq));
        }
        let input_queues_stats = Self::calculate_input_queues_stats(&canister_queues);
        let memory_usage_stats = Self::calculate_memory_usage_stats(&canister_queues);
        let output_queues_stats = Self::calculate_output_queues_stats(&canister_queues);

        let next_input_queue =
            match ProtoNextInputQueue::from_i32(item.next_input_queue).unwrap_or_default() {
                ProtoNextInputQueue::Unspecified | ProtoNextInputQueue::LocalSubnet => {
                    NextInputQueue::LocalSubnet
                }
                ProtoNextInputQueue::Ingress => NextInputQueue::Ingress,
                ProtoNextInputQueue::RemoteSubnet => NextInputQueue::RemoteSubnet,
            };

        let mut local_subnet_input_schedule = VecDeque::new();
        // Upgrade: input_schedule is mapped to local_subnet_input_schedule
        for can_id in item.input_schedule.into_iter() {
            let c = CanisterId::try_from(can_id)?;
            local_subnet_input_schedule.push_back(c);
        }
        for can_id in item.local_subnet_input_schedule.into_iter() {
            let c = CanisterId::try_from(can_id)?;
            local_subnet_input_schedule.push_back(c);
        }
        let mut remote_subnet_input_schedule = VecDeque::new();
        for can_id in item.remote_subnet_input_schedule.into_iter() {
            let c = CanisterId::try_from(can_id)?;
            remote_subnet_input_schedule.push_back(c);
        }

        Ok(Self {
            ingress_queue: IngressQueue::try_from(item.ingress_queue)?,
            canister_queues,
            input_queues_stats,
            output_queues_stats,
            memory_usage_stats,
            next_input_queue,
            local_subnet_input_schedule,
            remote_subnet_input_schedule,
        })
    }
}

/// Running message count and byte size stats across input queues.
///
/// Separate from [`MemoryUsageStats`] because the resulting `stats_delta()`
/// method would become quite cumbersome with an extra `QueueType` argument and
/// a `QueueOp` that only applied to memory usage stats; and would result in
/// adding lots of zeros in lots of places.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputQueuesStats {
    /// Count of messages in input queues.
    message_count: usize,

    /// Count of responses in input queues.
    response_count: usize,

    /// Count of reservations in input queue. Signed type because `stats_delta()`
    /// sometimes returns `-1`.
    reserved_slots: isize,

    /// Byte size of input queues (queues + messages).
    size_bytes: usize,

    /// Total amount of cycles contained in the input messages.
    cycles: Cycles,
}

impl InputQueuesStats {
    /// Calculates the change in input queue stats caused by pushing (+) or
    /// popping (-) the given message.
    fn stats_delta(op: QueueOp, msg: &RequestOrResponse) -> InputQueuesStats {
        let response_count = match msg {
            RequestOrResponse::Response(_) => 1,
            RequestOrResponse::Request(_) => 0,
        };
        // Consume one reservation iff pushing a response.
        let reserved_slots = match (op, msg) {
            (QueueOp::Push, RequestOrResponse::Response(_)) => -1,
            _ => 0,
        };

        InputQueuesStats {
            message_count: 1,
            response_count,
            reserved_slots,
            size_bytes: msg.count_bytes(),
            cycles: msg.cycles(),
        }
    }
}

impl AddAssign<InputQueuesStats> for InputQueuesStats {
    fn add_assign(&mut self, rhs: InputQueuesStats) {
        self.message_count += rhs.message_count;
        self.response_count += rhs.response_count;
        self.reserved_slots += rhs.reserved_slots;
        self.size_bytes += rhs.size_bytes;
        self.cycles += rhs.cycles;
    }
}

impl SubAssign<InputQueuesStats> for InputQueuesStats {
    fn sub_assign(&mut self, rhs: InputQueuesStats) {
        self.message_count -= rhs.message_count;
        self.response_count -= rhs.response_count;
        self.reserved_slots -= rhs.reserved_slots;
        self.size_bytes -= rhs.size_bytes;
        self.cycles -= rhs.cycles;
    }
}

/// Running stats across output queues.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputQueuesStats {
    /// Count of messages in output queues.
    message_count: usize,

    /// Total amount of cycles contained in the output queues.
    cycles: Cycles,
}

impl OutputQueuesStats {
    /// Calculates the change in output queue stats caused by pushing (+) or
    /// popping (-) the given message.
    fn stats_delta(msg: &RequestOrResponse) -> OutputQueuesStats {
        let cycles_message = match msg {
            RequestOrResponse::Response(response) => response.refund,
            RequestOrResponse::Request(request) => request.payment,
        };
        OutputQueuesStats {
            message_count: 1,
            cycles: cycles_message,
        }
    }
}

impl AddAssign<OutputQueuesStats> for OutputQueuesStats {
    fn add_assign(&mut self, rhs: OutputQueuesStats) {
        self.message_count += rhs.message_count;
        self.cycles += rhs.cycles;
    }
}

impl SubAssign<OutputQueuesStats> for OutputQueuesStats {
    fn sub_assign(&mut self, rhs: OutputQueuesStats) {
        self.message_count -= rhs.message_count;
        self.cycles -= rhs.cycles;
    }
}

/// Running memory utilization stats for input and output queues: total byte
/// size of all responses in input and output queues; and total reservations in
/// input and output queues.
///
/// Memory allocation of output responses in streams is tracked separately, at
/// the replicated state level (as the canister may be migrated to a different
/// subnet with outstanding responses still left in this subnet's streams).
///
/// Separate from [`InputQueuesStats`] because the resulting `stats_delta()`
/// method would become quite cumbersome with an extra `QueueType` argument and
/// a `QueueOp` that only applied to memory usage stats; and would result in
/// adding lots of zeros in lots of places.
#[derive(Clone, Debug, Default, Eq)]
struct MemoryUsageStats {
    /// Sum total of the byte size of every response across input and output
    /// queues.
    responses_size_bytes: usize,

    /// Sum total of reserved slots across input and output queues. This is
    /// equivalent to the number of outstanding (input and output) requests
    /// (across queues and streams) and is used for computing message memory
    /// allocation (as `MAX_RESPONSE_COUNT_BYTES` per request).
    ///
    /// `i64` because we need to be able to add negative amounts (e.g. pushing a
    /// response consumes a reservation) and it's less verbose this way.
    reserved_slots: i64,

    /// Sum total of bytes above `MAX_RESPONSE_COUNT_BYTES` per oversized
    /// request. Execution allows local-subnet requests larger than
    /// `MAX_RESPONSE_COUNT_BYTES`.
    oversized_requests_extra_bytes: usize,

    /// Transient: size in bytes of responses routed from `output_queues` into
    /// streams and not yet garbage collected.
    ///
    /// This is populated by `ReplicatedState::put_streams()`, called by MR
    /// after every streams mutation (induction, routing, GC).
    transient_stream_responses_size_bytes: usize,
}

impl MemoryUsageStats {
    /// Returns the memory usage in bytes computed from the stats.
    pub fn memory_usage(&self) -> usize {
        self.responses_size_bytes
            + self.reserved_slots as usize * MAX_RESPONSE_COUNT_BYTES
            + self.oversized_requests_extra_bytes
            + self.transient_stream_responses_size_bytes
    }

    /// Calculates the change in stats caused by pushing (+) or popping (-) the
    /// given message.
    fn stats_delta(op: QueueOp, msg: &RequestOrResponse) -> MemoryUsageStats {
        match msg {
            RequestOrResponse::Request(req) => Self::request_stats_delta(op, req),
            RequestOrResponse::Response(rep) => Self::response_stats_delta(op, rep),
        }
    }

    /// Calculates the change in stats caused by pushing (+) or popping (-) a
    /// request.
    fn request_stats_delta(op: QueueOp, req: &Request) -> MemoryUsageStats {
        MemoryUsageStats {
            // No change in responses byte size (as this is a request).
            responses_size_bytes: 0,
            // If we're pushing a request, we are reserving a slot.
            reserved_slots: match op {
                QueueOp::Push => 1,
                QueueOp::Pop => 0,
            },
            oversized_requests_extra_bytes: req
                .count_bytes()
                .saturating_sub(MAX_RESPONSE_COUNT_BYTES),
            transient_stream_responses_size_bytes: 0,
        }
    }

    /// Calculates the change in stats caused by pushing (+) or popping (-) the
    /// given response.
    fn response_stats_delta(op: QueueOp, rep: &Response) -> MemoryUsageStats {
        MemoryUsageStats {
            // Adjust responses byte size by this response's byte size.
            responses_size_bytes: rep.count_bytes(),
            // If we're pushing a response, we're consuming a reservation.
            reserved_slots: match op {
                QueueOp::Push => -1,
                QueueOp::Pop => 0,
            },
            // No change in requests overhead (as this is a response).
            oversized_requests_extra_bytes: 0,
            transient_stream_responses_size_bytes: 0,
        }
    }
}

impl AddAssign<MemoryUsageStats> for MemoryUsageStats {
    fn add_assign(&mut self, rhs: MemoryUsageStats) {
        self.responses_size_bytes += rhs.responses_size_bytes;
        self.reserved_slots += rhs.reserved_slots;
        self.oversized_requests_extra_bytes += rhs.oversized_requests_extra_bytes;
        debug_assert!(self.reserved_slots >= 0);
    }
}

impl SubAssign<MemoryUsageStats> for MemoryUsageStats {
    fn sub_assign(&mut self, rhs: MemoryUsageStats) {
        self.responses_size_bytes -= rhs.responses_size_bytes;
        self.reserved_slots -= rhs.reserved_slots;
        self.oversized_requests_extra_bytes -= rhs.oversized_requests_extra_bytes;
        debug_assert!(self.reserved_slots >= 0);
    }
}

// Custom `PartialEq`, ignoring `transient_stream_responses_size_bytes`.
impl PartialEq for MemoryUsageStats {
    fn eq(&self, rhs: &Self) -> bool {
        self.responses_size_bytes == rhs.responses_size_bytes
            && self.reserved_slots == rhs.reserved_slots
            && self.oversized_requests_extra_bytes == rhs.oversized_requests_extra_bytes
    }
}

/// Checks whether `available_memory` is sufficient to allow pushing `msg` onto
/// an input or output queue.
///
/// Returns:
///  * `Ok(())` if `msg` is a `Response`, as responses always return memory.
///  * `Ok(())` if `msg` is a `Request` and `available_memory` is sufficient.
///  * `Err(required_memory)` if `msg` is a `Request` and `required_memory >
///    available_memory`.
pub fn can_push(msg: &RequestOrResponse, available_memory: i64) -> Result<(), usize> {
    match msg {
        RequestOrResponse::Request(req) => {
            let required = memory_required_to_push_request(req);
            if required as i64 <= available_memory {
                Ok(())
            } else {
                Err(required)
            }
        }
        RequestOrResponse::Response(_) => Ok(()),
    }
}

/// Returns the memory required to push `req` onto an input or output queue.
/// This is the maximum of `MAX_RESPONSE_COUNT_BYTES` (to be reserved for a
/// response) and `req.count_bytes()` (if larger).
pub fn memory_required_to_push_request(req: &Request) -> usize {
    req.count_bytes().max(MAX_RESPONSE_COUNT_BYTES)
}

enum QueueOp {
    Push,
    Pop,
}

pub mod testing {
    use super::{CanisterQueues, MemoryUsageStats, QueueOp};
    use crate::canister_state::queues::OutputQueuesStats;
    use crate::{InputQueueType, StateError};
    use ic_interfaces::messages::CanisterInputMessage;
    use ic_types::{
        messages::{Request, RequestOrResponse},
        CanisterId, QueueIndex,
    };
    use std::{collections::VecDeque, sync::Arc};

    /// Exposes public testing-only `CanisterQueues` methods to be used in other
    /// crates' unit tests.
    pub trait CanisterQueuesTesting {
        /// Returns the number of messages in `ingress_queue`.
        fn ingress_queue_size(&self) -> usize;

        /// Pops the next message from the output queue associated with
        /// `dst_canister`. Returned `QueueIndex` values are sequential across
        /// successful calls.
        fn pop_canister_output(
            &mut self,
            dst_canister: &CanisterId,
        ) -> Option<(QueueIndex, RequestOrResponse)>;

        /// Returns the number of output queues, empty or not.
        fn output_queues_len(&self) -> usize;

        /// Returns the number of messages in `output_queues`.
        fn output_message_count(&self) -> usize;

        /// Publicly exposes `CanisterQueues::push_input()`.
        fn push_input(
            &mut self,
            index: QueueIndex,
            msg: RequestOrResponse,
            input_queue_type: InputQueueType,
        ) -> Result<(), (StateError, RequestOrResponse)>;

        /// Publicly exposes `CanisterQueues::pop_input()`.
        fn pop_input(&mut self) -> Option<CanisterInputMessage>;

        /// Publicly exposes the local subnet input_schedule.
        fn get_local_subnet_input_schedule(&self) -> &VecDeque<CanisterId>;

        /// Publicly exposes the remote subnet input_schedule.
        fn get_remote_subnet_input_schedule(&self) -> &VecDeque<CanisterId>;
    }

    impl CanisterQueuesTesting for CanisterQueues {
        fn ingress_queue_size(&self) -> usize {
            self.ingress_queue.size()
        }

        fn pop_canister_output(
            &mut self,
            dst_canister: &CanisterId,
        ) -> Option<(QueueIndex, RequestOrResponse)> {
            match self.canister_queues.get_mut(dst_canister) {
                None => None,
                Some((_, canister_out_queue)) => {
                    let ret = canister_out_queue.pop();
                    if let Some((_, msg)) = &ret {
                        self.output_queues_stats -= OutputQueuesStats::stats_delta(msg);
                        self.memory_usage_stats -= MemoryUsageStats::stats_delta(QueueOp::Pop, msg);
                    }
                    ret
                }
            }
        }

        fn output_queues_len(&self) -> usize {
            self.canister_queues.len()
        }

        fn output_message_count(&self) -> usize {
            self.canister_queues
                .values()
                .map(|(_, output_queue)| output_queue.num_messages())
                .sum()
        }

        fn push_input(
            &mut self,
            index: QueueIndex,
            msg: RequestOrResponse,
            input_queue_type: InputQueueType,
        ) -> Result<(), (StateError, RequestOrResponse)> {
            self.push_input(index, msg, input_queue_type)
        }

        fn pop_input(&mut self) -> Option<CanisterInputMessage> {
            self.pop_input()
        }

        fn get_local_subnet_input_schedule(&self) -> &VecDeque<CanisterId> {
            &self.local_subnet_input_schedule
        }

        fn get_remote_subnet_input_schedule(&self) -> &VecDeque<CanisterId> {
            &self.remote_subnet_input_schedule
        }
    }

    #[allow(dead_code)]
    /// Produces `CanisterQueues` together with a `VecDeque` of raw requests
    /// where the raw requests appear in the same order in the `VecDeque` as
    /// one would expect them being returned by the iterator.
    pub fn new_canister_queues_for_test(
        requests: Vec<Request>,
        sender: CanisterId,
        num_receivers: usize,
    ) -> (CanisterQueues, VecDeque<RequestOrResponse>) {
        let mut canister_queues = CanisterQueues::default();
        let mut updated_requests = VecDeque::new();
        requests.into_iter().enumerate().for_each(|(i, mut req)| {
            req.sender = sender;
            req.receiver = CanisterId::from_u64((i % num_receivers) as u64);
            let req = Arc::new(req);
            updated_requests.push_back(RequestOrResponse::Request(Arc::clone(&req)));
            canister_queues.push_output_request(req).unwrap();
        });
        (canister_queues, updated_requests)
    }
}
