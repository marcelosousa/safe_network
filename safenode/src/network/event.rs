// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::{
    error::{Error, Result},
    msg::MsgCodec,
    SwarmDriver,
};

use crate::{
    domain::storage::Chunk,
    protocol::{
        error::Error as ProtocolError,
        messages::{QueryResponse, Request, Response},
    },
};

use libp2p::{
    kad::{store::MemoryStore, GetRecordOk, Kademlia, KademliaEvent, QueryResult, K_VALUE},
    mdns,
    multiaddr::Protocol,
    request_response::{self, ResponseChannel as PeerResponseChannel},
    swarm::{NetworkBehaviour, SwarmEvent},
    Multiaddr, PeerId,
};
use std::collections::HashSet;
use tokio::sync::oneshot;
use tracing::{info, warn};

#[derive(NetworkBehaviour)]
#[behaviour(out_event = "NodeEvent")]
pub(super) struct NodeBehaviour {
    pub(super) request_response: request_response::Behaviour<MsgCodec>,
    pub(super) kademlia: Kademlia<MemoryStore>,
    pub(super) mdns: mdns::tokio::Behaviour,
}

#[derive(Debug)]
pub(super) enum NodeEvent {
    MsgReceived(request_response::Event<Request, Response>),
    Kademlia(KademliaEvent),
    Mdns(Box<mdns::Event>),
}

impl From<request_response::Event<Request, Response>> for NodeEvent {
    fn from(event: request_response::Event<Request, Response>) -> Self {
        NodeEvent::MsgReceived(event)
    }
}

impl From<KademliaEvent> for NodeEvent {
    fn from(event: KademliaEvent) -> Self {
        NodeEvent::Kademlia(event)
    }
}

impl From<mdns::Event> for NodeEvent {
    fn from(event: mdns::Event) -> Self {
        NodeEvent::Mdns(Box::new(event))
    }
}

#[derive(Debug)]
/// Channel to send the `Response` through.
pub enum MsgResponder {
    /// Respond to a request from `self` through a simple one-shot channel.
    FromSelf(oneshot::Sender<Result<Response>>),
    /// Respond to a request from a peer in the network.
    FromPeer(PeerResponseChannel<Response>),
}

#[derive(Debug)]
/// Events forwarded by the underlying Network; to be used by the upper layers
pub enum NetworkEvent {
    /// Incoming `Request` from a peer
    RequestReceived {
        /// Request
        req: Request,
        /// The channel to send the `Response` through
        channel: MsgResponder,
    },
    /// Emitted when the DHT is updated
    PeersAdded(Vec<PeerId>),
    /// Started listening on a new address
    NewListenAddr(Multiaddr),
}

impl SwarmDriver {
    // Handle `SwarmEvents`
    pub(super) async fn handle_swarm_events<EventError: std::error::Error>(
        &mut self,
        event: SwarmEvent<NodeEvent, EventError>,
    ) -> Result<()> {
        match event {
            SwarmEvent::Behaviour(NodeEvent::MsgReceived(event)) => {
                if let Err(e) = self.handle_msg(event).await {
                    warn!("MsgReceivedError: {e:?}");
                }
            }
            SwarmEvent::Behaviour(NodeEvent::Kademlia(ref event)) => match event {
                KademliaEvent::OutboundQueryProgressed {
                    id,
                    result: QueryResult::GetClosestPeers(Ok(closest_peers)),
                    stats,
                    step,
                } => {
                    trace!("Query task {id:?} returned with peers {closest_peers:?}, {stats:?} - {step:?}");

                    let (sender, mut current_closest) =
                        self.pending_get_closest_peers.remove(id).ok_or_else(|| {
                            trace!("Can't locate query task {id:?}, shall be completed already.");
                            Error::ReceivedKademliaEventDropped(event.clone())
                        })?;

                    // TODO: consider order the result and terminate when reach any of the
                    //       following creterias:
                    //   1, `stats.num_pending()` is 0
                    //   2, `stats.duration()` is longer than a defined period
                    let new_peers: HashSet<PeerId> =
                        closest_peers.peers.clone().into_iter().collect();
                    current_closest.extend(new_peers);
                    if current_closest.len() >= usize::from(K_VALUE) || step.last {
                        sender
                            .send(current_closest)
                            .map_err(|_| Error::InternalMsgChannelDropped)?;
                    } else {
                        let _ = self
                            .pending_get_closest_peers
                            .insert(*id, (sender, current_closest));
                    }
                }
                KademliaEvent::OutboundQueryProgressed {
                    id,
                    result: QueryResult::GetRecord(result),
                    stats,
                    step,
                } => {
                    trace!("Record query task {id:?} returned with result, {stats:?} - {step:?}");
                    if let Ok(GetRecordOk::FoundRecord(peer_record)) = result {
                        trace!(
                            "Query {id:?} returned with record {:?} from peer {:?}",
                            peer_record.record.key,
                            peer_record.peer
                        );
                        if let Some(sender) = self.pending_query.remove(id) {
                            sender
                                .send(QueryResponse::GetChunk(Ok(Chunk::new(
                                    peer_record.record.value.clone().into(),
                                ))))
                                .map_err(|_| Error::InternalMsgChannelDropped)?;
                        }
                    } else {
                        warn!("Query {id:?} failed to get record with result {result:?}");
                        if step.last {
                            // To avoid the caller wait forever on a non-existring entry
                            if let Some(sender) = self.pending_query.remove(id) {
                                sender
                                    .send(QueryResponse::GetChunk(Err(
                                        ProtocolError::RecordNotFound,
                                    )))
                                    .map_err(|_| Error::InternalMsgChannelDropped)?;
                            }
                        }
                        // TODO: send an error response back?
                    }
                }
                KademliaEvent::RoutingUpdated {
                    is_new_peer, peer, ..
                } => {
                    if *is_new_peer {
                        self.event_sender
                            .send(NetworkEvent::PeersAdded(vec![*peer]))
                            .await?;
                    }
                }
                KademliaEvent::InboundRequest { request } => {
                    info!("got inbound request: {request:?}");
                }
                todo => {
                    error!("KademliaEvent has not been implemented: {todo:?}");
                }
            },
            SwarmEvent::Behaviour(NodeEvent::Mdns(mdns_event)) => match *mdns_event {
                mdns::Event::Discovered(list) => {
                    let mut peers = vec![];
                    for (peer_id, multiaddr) in list {
                        info!("Node discovered: {multiaddr:?}");
                        let _routing_update = self
                            .swarm
                            .behaviour_mut()
                            .kademlia
                            .add_address(&peer_id, multiaddr);
                        peers.push(peer_id);
                    }
                    self.event_sender
                        .send(NetworkEvent::PeersAdded(peers))
                        .await?;
                }
                mdns::Event::Expired(peer) => {
                    info!("mdns peer {peer:?} expired");
                }
            },
            SwarmEvent::NewListenAddr { address, .. } => {
                let local_peer_id = *self.swarm.local_peer_id();
                let address = address.with(Protocol::P2p(local_peer_id.into()));
                self.event_sender
                    .send(NetworkEvent::NewListenAddr(address.clone()))
                    .await?;
                info!("Local node is listening on {address:?}");
            }
            SwarmEvent::IncomingConnection { .. } => {}
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                if endpoint.is_dialer() {
                    info!("Connected with {peer_id:?}");
                    if let Some(sender) = self.pending_dial.remove(&peer_id) {
                        let _ = sender.send(Ok(()));
                    }
                }
            }
            SwarmEvent::ConnectionClosed {
                peer_id, endpoint, ..
            } => {
                // The connection closed due to no msg flow show as `endpoint::receiver`
                // The connection closed due to peer dead show as `endpoint::dialer`
                info!("Connection closed to Peer {peer_id} - {endpoint:?}");
                if endpoint.is_dialer() {
                    info!("Dead Peer {peer_id:?}");
                    if self
                        .swarm
                        .behaviour_mut()
                        .kademlia
                        .remove_address(&peer_id, endpoint.get_remote_address())
                        .is_some()
                    {
                        info!("Removed dead peer {peer_id:?} from RT");
                    }
                }
            }
            SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                if let Some(peer_id) = peer_id {
                    if let Some(sender) = self.pending_dial.remove(&peer_id) {
                        let _ = sender.send(Err(error.into()));
                    }
                }
            }
            SwarmEvent::IncomingConnectionError { .. } => {}
            SwarmEvent::Dialing(peer_id) => info!("Dialing {peer_id}"),
            todo => error!("SwarmEvent has not been implemented: {todo:?}"),
        }
        Ok(())
    }
}
