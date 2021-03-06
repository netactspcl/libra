// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    block_storage::BlockStore,
    counters,
    liveness::{
        leader_reputation::{ActiveInactiveHeuristic, LeaderReputation, LibraDBBackend},
        proposal_generator::ProposalGenerator,
        proposer_election::ProposerElection,
        rotating_proposer_election::{choose_leader, RotatingProposer},
        round_state::{ExponentialTimeInterval, RoundState},
    },
    network::{IncomingBlockRetrievalRequest, NetworkReceivers, NetworkSender},
    network_interface::{ConsensusMsg, ConsensusNetworkSender},
    persistent_liveness_storage::{LedgerRecoveryData, PersistentLivenessStorage, RecoveryData},
    round_manager::{RecoveryManager, RoundManager, UnverifiedEvent, VerifiedEvent},
    state_replication::{StateComputer, TxnManager},
    util::time_service::TimeService,
};
use anyhow::{anyhow, bail, ensure, Context};
use channel::libra_channel;
use consensus_types::{
    common::{Author, Payload, Round},
    epoch_retrieval::EpochRetrievalRequest,
};
use futures::{select, StreamExt};
use libra_config::config::{ConsensusConfig, ConsensusProposerType, NodeConfig};
use libra_logger::prelude::*;
use libra_types::{
    account_address::AccountAddress,
    epoch_change::EpochChangeProof,
    epoch_state::EpochState,
    on_chain_config::{OnChainConfigPayload, ValidatorSet},
};
use network::protocols::network::Event;
use safety_rules::SafetyRulesManager;
use std::{
    cmp::Ordering,
    sync::Arc,
    time::{Duration, Instant},
};

/// RecoveryManager is used to process events in order to sync up with peer if we can't recover from local consensusdb
/// RoundManager is used for normal event handling.
/// We suppress clippy warning here because we expect most of the time we will have RoundManager
#[allow(clippy::large_enum_variant)]
pub enum RoundProcessor<T> {
    Recovery(RecoveryManager<T>),
    Normal(RoundManager<T>),
}

#[allow(clippy::large_enum_variant)]
pub enum LivenessStorageData<T> {
    RecoveryData(RecoveryData<T>),
    LedgerRecoveryData(LedgerRecoveryData),
}

impl<T: Payload> LivenessStorageData<T> {
    pub fn expect_recovery_data(self, msg: &str) -> RecoveryData<T> {
        match self {
            LivenessStorageData::RecoveryData(data) => data,
            LivenessStorageData::LedgerRecoveryData(_) => panic!("{}", msg),
        }
    }
}

// Manager the components that shared across epoch and spawn per-epoch RoundManager with
// epoch-specific input.
pub struct EpochManager<T> {
    author: Author,
    config: ConsensusConfig,
    time_service: Arc<dyn TimeService>,
    self_sender: channel::Sender<anyhow::Result<Event<ConsensusMsg<T>>>>,
    network_sender: ConsensusNetworkSender<T>,
    timeout_sender: channel::Sender<Round>,
    txn_manager: Box<dyn TxnManager<Payload = T>>,
    state_computer: Arc<dyn StateComputer<Payload = T>>,
    storage: Arc<dyn PersistentLivenessStorage<T>>,
    safety_rules_manager: SafetyRulesManager<T>,
    processor: Option<RoundProcessor<T>>,
}

impl<T: Payload> EpochManager<T> {
    pub fn new(
        node_config: &mut NodeConfig,
        time_service: Arc<dyn TimeService>,
        self_sender: channel::Sender<anyhow::Result<Event<ConsensusMsg<T>>>>,
        network_sender: ConsensusNetworkSender<T>,
        timeout_sender: channel::Sender<Round>,
        txn_manager: Box<dyn TxnManager<Payload = T>>,
        state_computer: Arc<dyn StateComputer<Payload = T>>,
        storage: Arc<dyn PersistentLivenessStorage<T>>,
    ) -> Self {
        let author = node_config.validator_network.as_ref().unwrap().peer_id;
        let config = node_config.consensus.clone();
        let safety_rules_manager = SafetyRulesManager::new(node_config);
        Self {
            author,
            config,
            time_service,
            self_sender,
            network_sender,
            timeout_sender,
            txn_manager,
            state_computer,
            storage,
            safety_rules_manager,
            processor: None,
        }
    }

    fn epoch_state(&self) -> &EpochState {
        match self
            .processor
            .as_ref()
            .expect("EpochManager not started yet")
        {
            RoundProcessor::Normal(p) => p.epoch_state(),
            RoundProcessor::Recovery(p) => p.epoch_state(),
        }
    }

    fn epoch(&self) -> u64 {
        self.epoch_state().epoch
    }

    fn create_round_state(
        &self,
        time_service: Arc<dyn TimeService>,
        timeout_sender: channel::Sender<Round>,
    ) -> RoundState {
        // 1.5^6 ~= 11
        // Timeout goes from initial_timeout to initial_timeout*11 in 6 steps
        let time_interval = Box::new(ExponentialTimeInterval::new(
            Duration::from_millis(self.config.round_initial_timeout_ms),
            1.5,
            6,
        ));
        RoundState::new(time_interval, time_service, timeout_sender)
    }

    /// Create a proposer election handler based on proposers
    fn create_proposer_election(
        &self,
        epoch_state: &EpochState,
    ) -> Box<dyn ProposerElection<T> + Send + Sync> {
        let proposers = epoch_state
            .verifier
            .get_ordered_account_addresses_iter()
            .collect::<Vec<_>>();
        match self.config.proposer_type {
            ConsensusProposerType::RotatingProposer => Box::new(RotatingProposer::new(
                proposers,
                self.config.contiguous_rounds,
            )),
            // We don't really have a fixed proposer!
            ConsensusProposerType::FixedProposer => {
                let proposer = choose_leader(proposers);
                Box::new(RotatingProposer::new(
                    vec![proposer],
                    self.config.contiguous_rounds,
                ))
            }
            ConsensusProposerType::LeaderReputation(heuristic_config) => {
                let backend = Box::new(LibraDBBackend::new(
                    proposers.len(),
                    self.storage.libra_db(),
                ));
                let heuristic = Box::new(ActiveInactiveHeuristic::new(
                    heuristic_config.active_weights,
                    heuristic_config.inactive_weights,
                ));
                Box::new(LeaderReputation::new(proposers, backend, heuristic))
            }
        }
    }

    async fn process_epoch_retrieval(
        &mut self,
        request: EpochRetrievalRequest,
        peer_id: AccountAddress,
    ) -> anyhow::Result<()> {
        let proof = self
            .storage
            .libra_db()
            .get_epoch_change_ledger_infos(request.start_epoch, request.end_epoch)
            .context("[EpochManager] Failed to get epoch proof")?;
        let msg = ConsensusMsg::EpochChangeProof::<T>(Box::new(proof));
        self.network_sender.send_to(peer_id, msg).context(format!(
            "[EpochManager] Failed to send epoch proof to {}",
            peer_id
        ))
    }

    async fn process_different_epoch(
        &mut self,
        different_epoch: u64,
        peer_id: AccountAddress,
    ) -> anyhow::Result<()> {
        match different_epoch.cmp(&self.epoch()) {
            // We try to help nodes that have lower epoch than us
            Ordering::Less => {
                self.process_epoch_retrieval(
                    EpochRetrievalRequest {
                        start_epoch: different_epoch,
                        end_epoch: self.epoch(),
                    },
                    peer_id,
                )
                .await
            }
            // We request proof to join higher epoch
            Ordering::Greater => {
                let request = EpochRetrievalRequest {
                    start_epoch: self.epoch(),
                    end_epoch: different_epoch,
                };
                let msg = ConsensusMsg::EpochRetrievalRequest::<T>(Box::new(request));
                self.network_sender.send_to(peer_id, msg).context(format!(
                    "[EpochManager] Failed to send epoch retrieval to {}",
                    peer_id
                ))
            }
            Ordering::Equal => {
                bail!("[EpochManager] Same epoch should not come to process_different_epoch");
            }
        }
    }

    async fn start_new_epoch(&mut self, proof: EpochChangeProof) -> anyhow::Result<()> {
        let ledger_info = proof
            .verify(self.epoch_state())
            .context("[EpochManager] Invalid EpochChangeProof")?;
        debug!(
            "Received epoch change to {}",
            ledger_info.ledger_info().epoch() + 1
        );

        // make sure storage is on this ledger_info too, it should be no-op if it's already committed
        self.state_computer
            .sync_to(ledger_info.clone())
            .await
            .context(format!(
                "[EpochManager] State sync to new epoch {}",
                ledger_info
            ))
        // state_computer notifies reconfiguration in another channel
    }

    async fn start_round_manager(
        &mut self,
        recovery_data: RecoveryData<T>,
        epoch_state: EpochState,
    ) {
        // Release the previous RoundManager, especially the SafetyRule client
        self.processor = None;
        counters::EPOCH.set(epoch_state.epoch as i64);
        counters::CURRENT_EPOCH_VALIDATORS.set(epoch_state.verifier.len() as i64);
        counters::CURRENT_EPOCH_QUORUM_SIZE.set(epoch_state.verifier.quorum_voting_power() as i64);
        info!(
            "Starting {} with genesis {}",
            epoch_state,
            recovery_data.root_block(),
        );
        let last_vote = recovery_data.last_vote();

        info!("Create BlockStore");
        let block_store = Arc::new(BlockStore::new(
            Arc::clone(&self.storage),
            recovery_data,
            Arc::clone(&self.state_computer),
            self.config.max_pruned_blocks_in_mem,
        ));

        info!("Update SafetyRules");

        let mut safety_rules = self.safety_rules_manager.client();
        let consensus_state = safety_rules
            .consensus_state()
            .expect("Unable to retrieve ConsensusState from SafetyRules");
        let sr_waypoint = consensus_state.waypoint();
        let proofs = self
            .storage
            .retrieve_epoch_change_proof(sr_waypoint.version())
            .expect("Unable to retrieve Waypoint state from Storage");

        safety_rules
            .initialize(&proofs)
            .expect("Unable to initialize SafetyRules");

        info!("Create ProposalGenerator");
        // txn manager is required both by proposal generator (to pull the proposers)
        // and by event processor (to update their status).
        let proposal_generator = ProposalGenerator::new(
            self.author,
            block_store.clone(),
            self.txn_manager.clone(),
            self.time_service.clone(),
            self.config.max_block_size,
        );

        info!("Create RoundState");
        let round_state =
            self.create_round_state(self.time_service.clone(), self.timeout_sender.clone());

        info!("Create ProposerElection");
        let proposer_election = self.create_proposer_election(&epoch_state);
        let network_sender = NetworkSender::new(
            self.author,
            self.network_sender.clone(),
            self.self_sender.clone(),
            epoch_state.verifier.clone(),
        );

        let mut processor = RoundManager::new(
            epoch_state,
            block_store,
            round_state,
            proposer_election,
            proposal_generator,
            safety_rules,
            network_sender,
            self.txn_manager.clone(),
            self.storage.clone(),
            self.time_service.clone(),
        );
        processor.start(last_vote).await;
        self.processor = Some(RoundProcessor::Normal(processor));
        info!("RoundManager started");
    }

    // Depending on what data we can extract from consensusdb, we may or may not have an
    // event processor at startup. If we need to sync up with peers for blocks to construct
    // a valid block store, which is required to construct an event processor, we will take
    // care of the sync up here.
    async fn start_recovery_manager(
        &mut self,
        ledger_recovery_data: LedgerRecoveryData,
        epoch_state: EpochState,
    ) {
        let network_sender = NetworkSender::new(
            self.author,
            self.network_sender.clone(),
            self.self_sender.clone(),
            epoch_state.verifier.clone(),
        );
        self.processor = Some(RoundProcessor::Recovery(RecoveryManager::new(
            epoch_state,
            network_sender,
            self.storage.clone(),
            self.state_computer.clone(),
            ledger_recovery_data.commit_round(),
        )));
        info!("SyncProcessor started");
    }

    pub async fn start_processor(&mut self, payload: OnChainConfigPayload) {
        let validator_set: ValidatorSet = payload
            .get()
            .expect("failed to get ValidatorSet from payload");
        let epoch_state = EpochState {
            epoch: payload.epoch(),
            verifier: (&validator_set).into(),
        };

        match self.storage.start() {
            LivenessStorageData::RecoveryData(initial_data) => {
                self.start_round_manager(initial_data, epoch_state).await
            }
            LivenessStorageData::LedgerRecoveryData(ledger_recovery_data) => {
                self.start_recovery_manager(ledger_recovery_data, epoch_state)
                    .await
            }
        }
    }

    pub async fn process_message(
        &mut self,
        peer_id: AccountAddress,
        consensus_msg: ConsensusMsg<T>,
    ) -> anyhow::Result<()> {
        if let Some(event) = self.process_epoch(peer_id, consensus_msg).await? {
            let verified_event = event
                .verify(&self.epoch_state().verifier)
                .context("[EpochManager] Verify event")?;
            self.process_event(peer_id, verified_event).await?;
        }
        Ok(())
    }

    async fn process_epoch(
        &mut self,
        peer_id: AccountAddress,
        msg: ConsensusMsg<T>,
    ) -> anyhow::Result<Option<UnverifiedEvent<T>>> {
        match msg {
            ConsensusMsg::ProposalMsg(_) | ConsensusMsg::SyncInfo(_) | ConsensusMsg::VoteMsg(_) => {
                let event: UnverifiedEvent<T> = msg.into();
                if event.epoch() == self.epoch() {
                    return Ok(Some(event));
                } else {
                    self.process_different_epoch(event.epoch(), peer_id).await?;
                }
            }
            ConsensusMsg::EpochChangeProof(proof) => {
                let msg_epoch = proof.epoch()?;
                if msg_epoch == self.epoch() {
                    self.start_new_epoch(*proof).await?;
                } else {
                    self.process_different_epoch(msg_epoch, peer_id).await?;
                }
            }
            ConsensusMsg::EpochRetrievalRequest(request) => {
                ensure!(
                    request.end_epoch <= self.epoch(),
                    "[EpochManager] Received EpochRetrievalRequest beyond what we have locally"
                );
                self.process_epoch_retrieval(*request, peer_id).await?;
            }
            _ => {
                bail!("[EpochManager] Unexpected messages: {:?}", msg);
            }
        }
        Ok(None)
    }

    async fn process_event(
        &mut self,
        peer_id: AccountAddress,
        event: VerifiedEvent<T>,
    ) -> anyhow::Result<()> {
        match self.processor_mut() {
            RoundProcessor::Recovery(p) => {
                let recovery_data = match event {
                    VerifiedEvent::ProposalMsg(proposal) => p.process_proposal_msg(*proposal).await,
                    VerifiedEvent::VoteMsg(vote) => p.process_vote(*vote).await,
                    _ => Err(anyhow!("Unexpected VerifiedEvent during startup")),
                }?;
                let epoch_state = p.epoch_state().clone();
                info!("Recovered from SyncProcessor");
                self.start_round_manager(recovery_data, epoch_state).await;
                Ok(())
            }
            RoundProcessor::Normal(p) => match event {
                VerifiedEvent::ProposalMsg(proposal) => p.process_proposal_msg(*proposal).await,
                VerifiedEvent::VoteMsg(vote) => p.process_vote(*vote).await,
                VerifiedEvent::SyncInfo(sync_info) => {
                    p.process_sync_info_msg(*sync_info, peer_id).await
                }
            },
        }
    }

    fn processor_mut(&mut self) -> &mut RoundProcessor<T> {
        self.processor
            .as_mut()
            .expect("[EpochManager] not started yet")
    }

    pub async fn process_block_retrieval(
        &mut self,
        request: IncomingBlockRetrievalRequest,
    ) -> anyhow::Result<()> {
        match self.processor_mut() {
            RoundProcessor::Normal(p) => p.process_block_retrieval(request).await,
            _ => bail!("[EpochManager] RoundManager not started yet"),
        }
    }

    pub async fn process_local_timeout(&mut self, round: u64) -> anyhow::Result<()> {
        match self.processor_mut() {
            RoundProcessor::Normal(p) => p.process_local_timeout(round).await,
            _ => unreachable!("RoundManager not started yet"),
        }
    }

    pub async fn start(
        mut self,
        mut round_timeout_sender_rx: channel::Receiver<Round>,
        mut network_receivers: NetworkReceivers<T>,
        mut reconfig_events: libra_channel::Receiver<(), OnChainConfigPayload>,
    ) {
        // initial start of the processor
        if let Some(payload) = reconfig_events.next().await {
            self.start_processor(payload).await;
        }
        loop {
            let pre_select_instant = Instant::now();
            let idle_duration;
            let result = select! {
                payload = reconfig_events.select_next_some() => {
                    idle_duration = pre_select_instant.elapsed();
                    self.start_processor(payload).await;
                    Ok(())
                }
                msg = network_receivers.consensus_messages.select_next_some() => {
                    idle_duration = pre_select_instant.elapsed();
                    self.process_message(msg.0, msg.1).await
                }
                block_retrieval = network_receivers.block_retrieval.select_next_some() => {
                    idle_duration = pre_select_instant.elapsed();
                    self.process_block_retrieval(block_retrieval).await
                }
                round = round_timeout_sender_rx.select_next_some() => {
                    idle_duration = pre_select_instant.elapsed();
                    self.process_local_timeout(round).await
                }
            };
            if let Err(e) = result {
                error!("{:?}", e);
            }
            if let RoundProcessor::Normal(p) = self.processor_mut() {
                debug!("{}", p.round_state());
            }
            counters::EVENT_PROCESSING_LOOP_BUSY_DURATION_S
                .observe_duration(pre_select_instant.elapsed() - idle_duration);
            counters::EVENT_PROCESSING_LOOP_IDLE_DURATION_S.observe_duration(idle_duration);
        }
    }
}
