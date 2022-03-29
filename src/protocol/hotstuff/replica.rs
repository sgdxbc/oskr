use std::{borrow::Borrow, collections::HashMap, ops::Index, sync::Arc};

use tracing::{debug, warn};

use crate::{
    common::{
        deserialize, serialize, signed::VerifiedMessage, ClientId, Digest, OpNumber, ReplicaId,
        RequestNumber, SignedMessage, SigningKey, VerifyingKey, ViewNumber,
    },
    facade::{App, Receiver, Transport, TxAgent},
    protocol::hotstuff::message::{self, GenericNode, QuorumCertification, ToReplica, GENESIS},
    stage::{Handle, State, StatefulContext, StatelessContext},
};

pub struct Replica<T: Transport> {
    address: T::Address,
    transport: T::TxAgent,
    id: ReplicaId,
    batch_size: usize,

    // although not present in event-driven HotStuff, current view must be kept,
    // so we have something to fill message field
    // the paper really didn't tell when to update, so probably in pacemaker, i
    // guess
    current_view: ViewNumber,
    vote_table: HashMap<Digest, HashMap<ReplicaId, SignedMessage<message::VoteGeneric>>>,
    voted_height: OpNumber,
    block_locked: Digest,
    block_executed: Digest,
    qc_high: QuorumCertification,
    block_leaf: Digest,

    client_table: HashMap<ClientId, (RequestNumber, Option<SignedMessage<message::Reply>>)>,
    log: HashMap<Digest, GenericNode>,
    batch: Vec<message::Request>,

    app: Box<dyn App + Send>,
    route_table: HashMap<ClientId, T::Address>,

    shared: Arc<Shared<T>>,
}

pub struct Shared<T: Transport> {
    address: T::Address,
    transport: T::TxAgent,

    signing_key: SigningKey,
    verifying_key: HashMap<T::Address, VerifyingKey>,
}

impl<T: Transport> Replica<T> {
    fn extend(&self, block: &GenericNode, ancestor: &Digest) -> bool {
        if &block.parent == ancestor {
            return true;
        }
        if let Some(parent) = self.log.get(&block.parent) {
            self.extend(parent, ancestor)
        } else {
            false
        }
    }
}

impl<D: Borrow<Digest>, T: Transport> Index<D> for Replica<T> {
    type Output = GenericNode;
    fn index(&self, index: D) -> &Self::Output {
        self.log.get(index.borrow()).unwrap()
    }
}

impl<T: Transport> State for Replica<T> {
    type Shared = Arc<Shared<T>>;
    fn shared(&self) -> Self::Shared {
        self.shared.clone()
    }
}

impl<'a, T: Transport> Receiver<T> for StatefulContext<'a, Replica<T>> {
    fn get_address(&self) -> &T::Address {
        &self.address
    }
}

impl<T: Transport> Receiver<T> for StatelessContext<Replica<T>> {
    fn get_address(&self) -> &T::Address {
        &self.address
    }
}

impl<T: Transport> Replica<T> {
    pub fn register_new(
        transport: &mut T,
        replica_id: ReplicaId,
        app: impl App + Send + 'static,
        batch_size: usize,
    ) -> Handle<Self> {
        assert!(transport.tx_agent().config().replica_address.len() > 1); // TODO

        let log = [(GENESIS.justify.node, GENESIS.clone())]
            .into_iter()
            .collect();

        let address = transport.tx_agent().config().replica_address[replica_id as usize].clone();
        let replica: Handle<_> = Self {
            address: address.clone(),
            transport: transport.tx_agent(),
            id: replica_id,
            batch_size,
            current_view: 0,
            vote_table: HashMap::new(),
            voted_height: 0,
            block_locked: GENESIS.justify.node,
            block_executed: GENESIS.justify.node,
            block_leaf: GENESIS.justify.node,
            qc_high: GENESIS.justify.clone(),
            client_table: HashMap::new(),
            log,
            batch: Vec::new(),
            app: Box::new(app),
            route_table: HashMap::new(),
            shared: Arc::new(Shared {
                signing_key: transport.tx_agent().config().signing_key[&address].clone(),
                verifying_key: transport.tx_agent().config().verifying_key(),
                address,
                transport: transport.tx_agent(),
            }),
        }
        .into();

        replica.with_stateful(|replica| {
            let submit = replica.submit.clone();
            transport.register(replica, move |remote, buffer| {
                submit.stateless(move |replica| replica.receive_buffer(remote, buffer));
            });
        });

        replica
    }
}

// "algorithm 4" in HotStuff paper
impl<T: Transport> StatefulContext<'_, Replica<T>> {
    // block3: b*, block2: b'', block1: b', block0: b
    fn update(&mut self, block3: &Digest) {
        let block2 = &{ self[block3].justify.node };
        let block1 = &{ self[block2].justify.node };
        let block0 = &{ self[block1].justify.node };

        let commit_block1 = self[block1].height > self[self.block_locked].height;
        let decide_block0 = self[block2].parent == *block1 && self[block1].parent == *block0;

        self.update_qc_high(self[block3].justify.clone());
        if commit_block1 {
            self.block_locked = *block1;
        }
        if decide_block0 {
            debug!("on commit: block = {:02x?}", block0);
            self.on_commit(block0);
            self.block_executed = *block0;
        }
    }

    fn on_commit(&mut self, block: &Digest) {
        if self[self.block_executed].height < self[block].height {
            self.on_commit(&{ self[block].parent });
            self.execute(block);
        }
    }
}
impl<T: Transport> StatelessContext<Replica<T>> {
    fn on_receive_proposal(&self, message: message::Generic) {
        let block_new = message.node;
        let digest = block_new.digest();
        self.submit.stateful(move |replica| {
            let safe_node = if replica.extend(&block_new, &replica.block_locked) {
                true
            } else if let Some(node) = replica.log.get(&block_new.justify.node) {
                node.height > replica[replica.block_locked].height
            } else {
                false
            };
            if block_new.height > replica.voted_height && safe_node {
                replica.voted_height = block_new.height;
                let vote_generic = message::VoteGeneric {
                    view_number: replica.current_view,
                    node: digest,
                    replica_id: replica.id,
                };

                let primary = replica.get_leader();
                replica.submit.stateless(move |replica| {
                    let signed = SignedMessage::sign(vote_generic, &replica.signing_key);
                    replica.transport.send_message_to_replica(
                        replica,
                        primary,
                        serialize(ToReplica::VoteGeneric(signed)),
                    );
                });
            }

            replica.log.insert(digest, block_new);
            replica.update(&digest);
        });
    }
}
impl<T: Transport> StatefulContext<'_, Replica<T>> {
    fn on_receive_vote(&mut self, message: VerifiedMessage<message::VoteGeneric>) {
        self.vote_table
            .entry(message.node)
            .or_default()
            .insert(message.replica_id, message.signed_message().clone());
        let vote_table = self.vote_table.get(&message.node).unwrap();
        if vote_table.len()
            >= self.transport.config().replica_address.len() - self.transport.config().n_fault
        {
            let qc = QuorumCertification {
                view_number: self.current_view,
                node: message.node,
                signature: vote_table.clone().into_iter().collect(),
            };
            self.update_qc_high(qc);
        }
    }

    // b_leaf and qc_high are read from state
    // returned b_new has to be delivered in CPS, and I blame HotStuff for that
    fn on_propose(
        &mut self,
        command: Vec<message::Request>,
        k: impl for<'a> FnOnce(&mut StatefulContext<'a, Replica<T>>, Digest) + Send + 'static,
    ) {
        let block_leaf = self.block_leaf;
        let qc_high = self.qc_high.clone();
        let height = self[&self.block_leaf].height + 1;
        let view_number = self.current_view;
        self.submit.stateless(move |replica| {
            let block_new = GenericNode::create_leaf(&block_leaf, command, qc_high, height);
            let generic = message::Generic {
                view_number,
                node: block_new.clone(),
            };
            replica
                .transport
                .send_message_to_all(replica, serialize(ToReplica::Generic(generic)));

            let digest = block_new.digest();
            replica.submit.stateful(move |replica| {
                replica.log.insert(digest, block_new);
                k(replica, digest);
            });
        });
    }
}
// "algorithm 5" in HotStuff paper
impl<T: Transport> StatefulContext<'_, Replica<T>> {
    fn get_leader(&self) -> ReplicaId {
        self.transport.config().view_primary(self.current_view)
    }

    fn update_qc_high(&mut self, qc_high1: QuorumCertification) {
        if self[&qc_high1.node].height > self[&self.qc_high.node].height {
            self.block_leaf = qc_high1.node;
            self.qc_high = qc_high1;
        }
    }

    pub(super) fn on_beat(&mut self, command: Vec<message::Request>) {
        debug!("on beat");
        if self.get_leader() == self.id {
            self.on_propose(command, |replica, block_leaf| {
                replica.block_leaf = block_leaf;
            });
        }
    }

    // TODO new view
}

// the other thing to support
impl<T: Transport> StatelessContext<Replica<T>> {
    fn receive_buffer(&self, remote: T::Address, buffer: T::RxBuffer) {
        match deserialize(buffer.as_ref()) {
            Ok(ToReplica::Request(request)) => {
                self.submit
                    .stateful(move |replica| replica.handle_request(remote, request));
                return;
            }
            Ok(ToReplica::Generic(generic)) => {
                let verifying_key = |replica| {
                    &self.verifying_key[&self.transport.config().replica_address[replica as usize]]
                };
                let threshold =
                    self.transport.config().replica_address.len() - self.transport.config().n_fault;
                if generic
                    .node
                    .justify
                    .verify(verifying_key, threshold)
                    .is_err()
                {
                    warn!("failed to verify generic node justify");
                    return;
                }

                self.on_receive_proposal(generic);
                return;
            }
            Ok(ToReplica::VoteGeneric(vote_generic)) => {
                if let Ok(verified) = vote_generic.verify(&self.verifying_key[&remote]) {
                    self.submit.stateful(move |replica| {
                        if verified.view_number == replica.current_view {
                            replica.on_receive_vote(verified);
                        }
                    });
                } else {
                    warn!("failed to verify vote generic");
                }
                return;
            }
            _ => {}
        }
        warn!("failed to deserialize");
    }
}
impl<T: Transport> StatefulContext<'_, Replica<T>> {
    fn handle_request(&mut self, remote: T::Address, message: message::Request) {
        self.route_table.insert(message.client_id, remote.clone());

        if let Some((request_number, reply)) = self.client_table.get(&message.client_id) {
            if *request_number > message.request_number {
                return;
            }
            if *request_number == message.request_number {
                if let Some(reply) = reply {
                    self.transport
                        .send_message(self, &remote, serialize(reply.clone()));
                }
                return;
            }
        }

        self.batch.push(message);
        if self.batch.len() == self.batch_size {
            let command = self.batch.drain(..).collect();
            self.on_beat(command);
        }
    }

    fn execute(&mut self, block: &Digest) {
        for request in self[block].command.clone() {
            if let Some((request_number, _)) = self.client_table.get(&request.client_id) {
                if *request_number >= request.request_number {
                    continue;
                }
            }

            let result = self.app.execute(request.op.clone());
            let reply = message::Reply {
                request_number: request.request_number,
                result,
                replica_id: self.id,
            };

            let remote = self.route_table.get(&request.client_id).cloned();
            let client_id = request.client_id;
            let request_number = request.request_number;
            self.submit.stateless(move |replica| {
                let signed = SignedMessage::sign(reply, &replica.signing_key);
                if let Some(remote) = remote {
                    replica
                        .transport
                        .send_message(replica, &remote, serialize(&signed));
                } else {
                    warn!("no route record so skip sending reply");
                }
                replica.submit.stateful(move |replica| {
                    replica
                        .client_table
                        .insert(client_id, (request_number, Some(signed)));
                });
            });
        }
    }
}