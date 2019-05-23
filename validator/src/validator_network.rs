use std::collections::btree_map::BTreeMap;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

use failure::Fail;
use parking_lot::RwLock;

use block_albatross::{
    ForkProof, PbftProof, PbftProofBuilder, PbftProposal,
    SignedPbftCommitMessage, SignedPbftPrepareMessage, SignedPbftProposal,
    SignedViewChange, ViewChange, ViewChangeProof, ViewChangeProofBuilder
};
use blockchain_albatross::Blockchain;
use bls::bls12_381::{CompressedPublicKey, PublicKey};
use hash::{Blake2bHash, Hash};
use messages::Message;
use network::{Network, NetworkEvent, Peer};
use network_primitives::address::PeerId;
use network_primitives::validator_info::{SignedValidatorInfo, ValidatorId};
use primitives::policy::TWO_THIRD_VALIDATORS;
use utils::mutable_once::MutableOnce;
use utils::observer::{PassThroughNotifier, weak_listener, weak_passthru_listener};

use crate::validator_agent::{ValidatorAgent, ValidatorAgentEvent};

#[derive(Clone, Debug, Fail)]
pub enum ValidatorNetworkError {
    #[fail(display = "pBFT already started with different proposal")]
    IncorrectProposal,
    #[fail(display = "Not in pBFT voting phase")]
    NotInPbftPhase
}

pub enum ValidatorNetworkEvent {
    /// When a fork proof was given
    ForkProof(ForkProof),

    /// When a valid view change was completed
    ViewChangeComplete(ViewChange, ViewChangeProof),

    /// When a valid macro block is proposed by the correct pBFT-leader. This can happen multiple
    /// times during an epoch - i.e. when a proposal with a higher view number is received.
    PbftProposal(Blake2bHash, PbftProposal),

    /// When enough prepare signatures are collected for a proposed macro block
    PbftPrepareComplete(Blake2bHash, PbftProposal),

    /// When enough commit signatures from signers that also signed prepare messages are collected
    PbftCommitComplete(Blake2bHash, PbftProposal, PbftProof),
}

struct ValidatorNetworkState {
    /// The peers that are connected that have the validator service flag set. So this is not
    /// exactly the set of validators. Potential validators should set this flag and then broadcast
    /// a `ValidatorInfo`.
    agents: HashMap<PeerId, Arc<RwLock<ValidatorAgent>>>,

    /// Peers for which we received a `ValidatorInfo` and thus have a BLS public key
    validators: HashMap<ValidatorId, Arc<RwLock<ValidatorAgent>>>,

    /// Subset of validators that only includes validators that are active in the current epoch.
    active: HashMap<ValidatorId, Arc<RwLock<ValidatorAgent>>>,

    /// maps (view-change-number, block-number) to the proof that is being aggregated
    /// clear after macro block
    view_changes: BTreeMap<ViewChange, ViewChangeProofBuilder>,

    /// The current proposed macro header and pbft proof.
    ///
    /// This exists between proposal and macro block finalized. The header hash is stored for
    /// efficiency reasons.
    pbft_proof: Option<(PbftProposal, Blake2bHash, PbftProofBuilder)>,
}

pub struct ValidatorNetwork {
    network: Arc<Network<Blockchain<'static>>>,
    blockchain: Arc<Blockchain<'static>>,

    info: SignedValidatorInfo,

    state: RwLock<ValidatorNetworkState>,

    self_weak: MutableOnce<Weak<ValidatorNetwork>>,
    pub notifier: RwLock<PassThroughNotifier<'static, ValidatorNetworkEvent>>,
}

impl ValidatorNetwork {
    pub fn new(network: Arc<Network<Blockchain<'static>>>, blockchain: Arc<Blockchain<'static>>, info: SignedValidatorInfo) -> Arc<Self> {
        let this = Arc::new(ValidatorNetwork {
            network,
            blockchain,
            info,
            state: RwLock::new(ValidatorNetworkState {
                agents: HashMap::new(),
                validators: HashMap::new(),
                active: HashMap::new(),
                view_changes: BTreeMap::new(),
                pbft_proof: None,
            }),
            self_weak: MutableOnce::new(Weak::new()),
            notifier: RwLock::new(PassThroughNotifier::new()),
        });

        Self::init_listeners(&this);

        this
    }

    fn init_listeners(this: &Arc<Self>) {
        unsafe { this.self_weak.replace(Arc::downgrade(this)) };

        this.network.notifier.write().register(weak_listener(Arc::downgrade(this), |this, event| {
            match event {
                NetworkEvent::PeerJoined(peer) => this.on_peer_joined(&peer),
                NetworkEvent::PeerLeft(peer) => this.on_peer_left(&peer),
                _ => {}
            }
        }));
    }

    fn on_peer_joined(&self, peer: &Arc<Peer>) {
        if peer.peer_address().services.is_validator() {
            let agent = ValidatorAgent::new(Arc::clone(peer), Arc::clone(&self.blockchain));

            // insert into set of all agents that have the validator service flag
            self.state.write().agents.insert(peer.peer_address().peer_id.clone(), Arc::clone(&agent));

            // register for messages received by agent
            agent.read().notifier.write().register(weak_passthru_listener(Weak::clone(&self.self_weak), |this, event| {
                match event {
                    ValidatorAgentEvent::ValidatorInfo(info) => {
                        this.on_validator_info(info);
                    },
                    ValidatorAgentEvent::ForkProof(fork_proof) => {
                        this.on_fork_proof(fork_proof);
                    }
                    ValidatorAgentEvent::ViewChange { view_change, public_key, slots } => {
                        this.commit_view_change(view_change, &public_key, slots)
                            .unwrap_or_else(|e| warn!("Failed to commit view change: {}", e));
                    },
                    ValidatorAgentEvent::PbftProposal(proposal) => {
                        this.commit_pbft_proposal(proposal)
                            .unwrap_or_else(|e| warn!("Failed to commit pBFT proposal: {}", e));
                    },
                    ValidatorAgentEvent::PbftPrepare { prepare, public_key, slots } => {
                        this.commit_pbft_prepare(prepare, &public_key, slots)
                            .unwrap_or_else(|e| warn!("Failed to commit pBFT prepare: {}", e));
                    },
                    ValidatorAgentEvent::PbftCommit { commit, public_key, slots } => {
                        this.commit_pbft_commit(commit, &public_key, slots)
                            .unwrap_or_else(|e| warn!("Failed to commit pBFT commit: {}", e));
                    },
                }
            }));

            // send known validator infos to peer
            let mut infos = self.state.read().validators.iter()
                .take(64) // only send 64
                .filter_map(|(_, validator)| {
                    validator.read().validator_info.clone()
                }).collect::<Vec<SignedValidatorInfo>>();
            infos.push(self.info.clone());
            peer.channel.send_or_close(Message::ValidatorInfo(infos));
        }
    }

    fn on_peer_left(&self, peer: &Arc<Peer>) {
        let mut state = self.state.write();

        if let Some(agent) = state.agents.remove(&peer.peer_address().peer_id) {
            if let Some(info) = &agent.read().validator_info {
                state.validators.remove(&info.message.validator_id);
                state.active.remove(&info.message.validator_id);
            }
        }
    }

    fn on_validator_info(&self, info: SignedValidatorInfo) {
        let mut state = self.state.write();

        trace!("Validator info: {:?}", info.message);

        if let Some(agent) = state.agents.get(&info.message.peer_address.peer_id) {
            let agent = Arc::clone(&agent);

            if let Some(current_info) = &agent.read().validator_info {
                if current_info.message.validator_id == info.message.validator_id {
                    // didn't change, do nothing
                    return;
                }

                // if the validator ID changed, remove peer from validator agents first
                state.validators.remove(&current_info.message.validator_id);
            }

            // add peer to validator agents
            state.validators.insert(info.message.validator_id.clone(), Arc::clone(&agent));

            // TODO: check if active validator and put into `active` list

            // put validator info into agent
            agent.write().validator_info = Some(info);
        }
        else {
            debug!("ValidatorInfo for unknown peer: {:?}", info);
        }
    }

    fn on_fork_proof(&self, fork_proof: ForkProof) {
        self.notifier.read().notify(ValidatorNetworkEvent::ForkProof(fork_proof.clone()));
        self.broadcast_fork_proof(fork_proof);
    }

    fn broadcast_fork_proof(&self, fork_proof: ForkProof) {
        self.broadcast_active(Message::ForkProof(Box::new(fork_proof)));
    }

    /// Called when we reach finality - i.e. when a macro block was produced
    /// TODO: register in consensus for macro blocks
    /// TODO: Remove validators with keys that don't have stake anymore?
    /// TODO: Compute set of validator agents that are now active.
    pub fn on_finality(&self) {
        debug!("Clearing view change and pBFT proof");

        let mut state = self.state.write();

        state.view_changes.clear();
        state.pbft_proof = None;
        state.active.clear();
    }

    /// Commit a view change to the proofs being build and relay it if it's new
    pub fn commit_view_change(&self, view_change: SignedViewChange, public_key: &PublicKey, slots: u16) -> Result<(), ValidatorNetworkError> {
        let mut state = self.state.write();

        trace!("Commit view change: {:?}", view_change);

        // get the proof with the specific block number and view change number
        // if it doesn't exist, create a new one.
        let proof = state.view_changes.entry(view_change.message.clone())
            .or_insert_with(|| ViewChangeProofBuilder::new());

        // Aggregate signature - if it wasn't included yet, relay it
        if proof.add_signature(public_key, slots, &view_change) {
            let proof_complete = proof.verify(&view_change.message, TWO_THIRD_VALIDATORS).is_ok();
            trace!("Added signature to view change proof: votes={}, complete={}", proof.num_slots, proof_complete);

            // if we have enough signatures, notify listeners
            if proof_complete {
                let proof = proof.clone().build();

                drop(state); // drop before notify and broadcast

                self.notifier.read()
                    .notify(ValidatorNetworkEvent::ViewChangeComplete(view_change.message.clone(), proof))
            }
            else {
                drop(state);
            }

            // broadcast new view change signature
            self.broadcast_active(Message::ViewChange(Box::new(view_change.clone())));
        }

        Ok(())
    }

    /// Commit a macro block proposal
    pub fn commit_pbft_proposal(&self, proposal: SignedPbftProposal) -> Result<(), ValidatorNetworkError> {
        let mut state = self.state.write();

        let commit = if let Some((current_proposal, _, _)) = &state.pbft_proof {
            if *current_proposal == proposal.message {
                // if we already know the proposal, ignore it
                false
            }
            else if proposal.message.header.view_number <= current_proposal.header.view_number {
                // if it has a lower view number than the current one, ignore it
                debug!("Ignoring new macro block proposal with lower view change: {:#?}", proposal.message);
                false
            }
            else {
                // if it has a higher view number, commit it
                debug!("New macro block proposal with higher view change: {:#?}", proposal.message);
                true
            }
        }
        else {
            // if we don't have a proposal yet, commit it
            debug!("New macro block proposal: {:#?}", proposal.message);
            true
        };

        if commit {
            // remember proposal
            let block_hash = proposal.message.header.hash::<Blake2bHash>();
            state.pbft_proof = Some((proposal.message.clone(), block_hash.clone(), PbftProofBuilder::new()));

            // drop lock
            drop(state);

            // notify Jeff, a.k.a notify `Validator`
            self.notifier.read().notify(ValidatorNetworkEvent::PbftProposal(block_hash, proposal.message.clone()));

            // relay proposal
            self.broadcast_active(Message::PbftProposal(Box::new(proposal)));
        }

        Ok(())
    }

    /// Commit a pBFT prepare
    pub fn commit_pbft_prepare(&self, prepare: SignedPbftPrepareMessage, public_key: &PublicKey, slots: u16) -> Result<(), ValidatorNetworkError> {
        let mut state = self.state.write();

        if let Some((proposal, block_hash, proof)) = &mut state.pbft_proof {
            // check if this prepare is for our current proposed block
            if prepare.message.block_hash != *block_hash {
                debug!("Prepare for unknown block: {}", prepare.message.block_hash);
                return Err(ValidatorNetworkError::IncorrectProposal);
            }

            // aggregate prepare signature - if new, relay
            if proof.add_prepare_signature(public_key, slots, &prepare) {
                let prepare_complete = proof.prepare.verify(&prepare.message, TWO_THIRD_VALIDATORS).is_ok();
                let commit_complete = proof.verify(prepare.message.block_hash.clone(), &self.blockchain.current_validators(), TWO_THIRD_VALIDATORS).is_ok();

                trace!("Committing pBFT prepare: prepare_complete={}, commit_complete={}", prepare_complete, commit_complete);

                // XXX Can we get rid of the eager cloning here?
                let proposal = proposal.clone();
                let proof = proof.clone().build();

                // drop lock before notifying and broadcasting
                drop(state);

                // notify if we reach threshold on prepare to begin commit
                if prepare_complete {
                    self.notifier.read()
                        .notify(ValidatorNetworkEvent::PbftPrepareComplete(prepare.message.block_hash.clone(), proposal.clone()))
                }

                // NOTE: It might happen that we receive the prepare message after the commit. So we have
                //       to verify here too.
                if commit_complete {
                    self.notifier.read()
                        .notify(ValidatorNetworkEvent::PbftCommitComplete(prepare.message.block_hash.clone(), proposal, proof))
                }

                // broadcast new pbft prepare signature
                self.broadcast_active(Message::PbftPrepare(Box::new(prepare)));
            }
            Ok(())
        }
        else {
            debug!("Not in pBFT phase");
            Err(ValidatorNetworkError::NotInPbftPhase)
        }
    }

    /// Commit a pBFT commit
    pub fn commit_pbft_commit(&self, commit: SignedPbftCommitMessage, public_key: &PublicKey, slots: u16) -> Result<(), ValidatorNetworkError> {
        let mut state = self.state.write();

        if let Some((proposal, block_hash, proof)) = &mut state.pbft_proof {
            // check if this prepare is for our current proposed block
            if commit.message.block_hash != *block_hash {
                debug!("Prepare for unknown block: {}", block_hash);
                return Err(ValidatorNetworkError::IncorrectProposal);
            }

            // aggregate commit signature - if new, relay
            if proof.add_commit_signature(public_key, slots, &commit) {
                let commit_complete = proof.verify(commit.message.block_hash.clone(), &self.blockchain.current_validators(), TWO_THIRD_VALIDATORS).is_ok();

                trace!("Committing pBFT commit: commit_complete={}", commit_complete);

                // XXX Can we get rid of the eager cloning here?
                let proposal = proposal.clone();
                let proof = proof.clone().build();

                // drop lock before notifying
                drop(state);

                if commit_complete {
                    self.notifier.read()
                        .notify(ValidatorNetworkEvent::PbftCommitComplete(commit.message.block_hash.clone(), proposal, proof));
                }

                // broadcast new pbft commit signature
                self.broadcast_active(Message::PbftCommit(Box::new(commit)));
            }
            Ok(())
        }
        else {
            debug!("Not in pBFT phase");
            Err(ValidatorNetworkError::NotInPbftPhase)
        }
    }

    /// Broadcast to all known active validators
    pub fn broadcast_active(&self, msg: Message) {
        for (_, agent) in self.state.read().active.iter() {
            agent.read().peer.channel.send_or_close(msg.clone())
        }
    }

    /// Broadcast to all known validators
    pub fn broadcast_all(&self, msg: Message) {
        for (_, agent) in self.state.read().validators.iter() {
            agent.read().peer.channel.send_or_close(msg.clone());
        }
    }

    /// Broadcast our own validator info
    pub fn broadcast_info(&self, info: SignedValidatorInfo) {
        self.broadcast_all(Message::ValidatorInfo(vec![info]));
    }
}
