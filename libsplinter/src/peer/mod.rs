// Copyright 2018-2022 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Data structures and implementations for managing Splinter peers.
//!
//! The public interface includes the structs [`PeerManager`], [`PeerManagerConnector`],
//! [`PeerInterconnect`] and the enum [`PeerManagerNotification`].
//!
//! [`PeerManager`]: struct.PeerManager.html
//! [`PeerManagerConnector`]: connector/struct.PeerManagerConnector.html
//! [`PeerInterconnect`]: interconnect/struct.PeerInterconnect.html
//! [`PeerManagerNotification`]: notification/enum.PeerManagerNotification.html

mod builder;
mod connector;
mod error;
pub mod interconnect;
mod notification;
mod peer_map;
mod peer_ref;
mod token;
mod unreferenced;

use std::cmp::min;
use std::io::ErrorKind;
use std::sync::mpsc::{channel, Sender};
use std::thread;
use std::time::Instant;

use uuid::Uuid;

use crate::collections::{BiHashMap, RefMap};
use crate::error::InternalError;
use crate::network::connection_manager::ConnectionManagerNotification;
use crate::network::connection_manager::{ConnectionManagerError, Connector};
use crate::threading::lifecycle::ShutdownHandle;
use crate::threading::pacemaker;

pub use self::builder::PeerManagerBuilder;
use self::connector::PeerRemover;
pub use self::connector::{PeerLookup, PeerManagerConnector};
use self::error::{
    PeerConnectionIdError, PeerListError, PeerLookupError, PeerManagerError, PeerRefAddError,
    PeerRefRemoveError, PeerUnknownAddError,
};
pub use self::notification::{PeerManagerNotification, PeerNotificationIter, SubscriberId};
use self::notification::{Subscriber, SubscriberMap};
use self::peer_map::{PeerMap, PeerStatus};
pub use self::peer_ref::{EndpointPeerRef, PeerRef};
pub use self::token::{PeerAuthorizationToken, PeerTokenPair};
use self::unreferenced::{RequestedEndpoint, UnreferencedPeer, UnreferencedPeerState};

/// Internal messages to drive management
pub(crate) enum PeerManagerMessage {
    /// Notifies the `PeerManger` it should shutdown
    Shutdown,
    /// Sent from the `PeerManagerConnector` to add peers
    Request(PeerManagerRequest),
    /// Used to subscribe to `PeerManagerNotification`
    Subscribe(Sender<PeerManagerNotification>),
    /// Passes `ConnectionManagerNotification` to the `PeerManger` for handling
    InternalNotification(ConnectionManagerNotification),
    /// Notifies the `PeerManager` it should retry connecting to pending peers
    RetryPending,
}

/// Converts `ConnectionManagerNotification` into `PeerManagerMessage::InternalNotification`
impl From<ConnectionManagerNotification> for PeerManagerMessage {
    fn from(notification: ConnectionManagerNotification) -> Self {
        PeerManagerMessage::InternalNotification(notification)
    }
}

/// The requests that will be handled by the `PeerManager`
pub(crate) enum PeerManagerRequest {
    AddPeer {
        peer_id: PeerAuthorizationToken,
        endpoints: Vec<String>,
        required_local_auth: PeerAuthorizationToken,
        sender: Sender<Result<PeerRef, PeerRefAddError>>,
    },
    AddUnidentified {
        endpoint: String,
        local_authorization: PeerAuthorizationToken,
        sender: Sender<Result<EndpointPeerRef, PeerUnknownAddError>>,
    },
    RemovePeer {
        peer_id: PeerTokenPair,
        sender: Sender<Result<(), PeerRefRemoveError>>,
    },
    RemovePeerByEndpoint {
        endpoint: String,
        connection_id: String,
        sender: Sender<Result<(), PeerRefRemoveError>>,
    },
    ListPeers {
        sender: Sender<Result<Vec<PeerAuthorizationToken>, PeerListError>>,
    },
    ListUnreferencedPeers {
        sender: Sender<Result<Vec<PeerTokenPair>, PeerListError>>,
    },
    ConnectionIds {
        sender: Sender<Result<BiHashMap<PeerTokenPair, String>, PeerConnectionIdError>>,
    },
    GetConnectionId {
        peer_id: PeerTokenPair,
        sender: Sender<Result<Option<String>, PeerLookupError>>,
    },
    GetPeerId {
        connection_id: String,
        sender: Sender<Result<Option<PeerTokenPair>, PeerLookupError>>,
    },
    Subscribe {
        sender: Sender<Result<SubscriberId, PeerManagerError>>,
        callback: Subscriber,
    },
    Unsubscribe {
        subscriber_id: SubscriberId,
        sender: Sender<Result<(), PeerManagerError>>,
    },
}

/// The `PeerManager` is in charge of keeping track of peers and their reference counts, as well as
/// requesting connections from the `ConnectionManager`. If a peer has disconnected, the
/// `PeerManager` will also try the peer's other endpoints until one is successful.
pub struct PeerManager {
    join_handle: thread::JoinHandle<()>,
    sender: Sender<PeerManagerMessage>,
    pacemaker_shutdown_signaler: pacemaker::ShutdownSignaler,
}

impl PeerManager {
    /// Creates a new `PeerManager`
    ///
    /// # Arguments
    ///
    /// * `connector` - The `Connector` to the `ConnectionManager` that will handle the connections
    ///    requested by the `PeerManager`
    /// * `max_retry_attempts` - The number of retry attempts for an active endpoint before the
    ///    `PeerManager` will try other endpoints associated with a peer
    /// * `retry_interval` - How often (in seconds) the `Pacemaker` should notify the `PeerManager`
    ///    to retry pending peers
    /// * `identity` - The unique ID of the node this `PeerManager` belongs to
    /// * `strict_ref_counts` - Determines whether or not to panic when attempting to remove a
    ///   reference to peer that is not referenced.
    #[deprecated(since = "0.5.1", note = "Please use PeerManagerBuilder instead")]
    pub fn new(
        connector: Connector,
        max_retry_attempts: Option<u64>,
        retry_interval: Option<u64>,
        identity: String,
        strict_ref_counts: bool,
    ) -> Self {
        let mut builder = PeerManagerBuilder::default()
            .with_connector(connector)
            .with_identity(identity)
            .with_strict_ref_counts(strict_ref_counts);

        if let Some(max_retry) = max_retry_attempts {
            builder = builder.with_max_retry_attempts(max_retry);
        }

        if let Some(retry_interval) = retry_interval {
            builder = builder.with_retry_interval(retry_interval);
        }

        // This should never fail due to the required values of the new function
        builder
            .start()
            .expect("Building the PeerManager failed unexpectedly")
    }

    /// Construct a new `PeerManagerBuilder` for creating a new `PeerManager` instance.
    pub fn builder() -> PeerManagerBuilder {
        PeerManagerBuilder::default()
    }

    /// Starts the `PeerManager`
    ///
    /// Starts up a thread that will handle incoming requests to add, remove and get peers. Also
    /// handles notifications from the `ConnectionManager`.
    ///
    /// Returns a `PeerManagerConnector` that can be used to send requests to the `PeerManager`.
    #[deprecated(
        since = "0.5.1",
        note = "Please use connector() instead. The PeerManagerBuilder starts up the PeerManager \
         now"
    )]
    pub fn start(&mut self) -> Result<PeerManagerConnector, PeerManagerError> {
        Ok(PeerManagerConnector::new(self.sender.clone()))
    }

    pub fn connector(&self) -> PeerManagerConnector {
        PeerManagerConnector::new(self.sender.clone())
    }

    /// Private constructor used by the builder to start the peer manager
    #[allow(clippy::too_many_arguments)]
    // Allow clippy errors for too_many_arguments. This method is private and is in support of the
    // PeerManagerBuilder.
    fn build(
        retry_interval: u64,
        max_retry_attempts: u64,
        strict_ref_counts: bool,
        // identity is not used if challenge-authorization is enabled
        #[allow(unused_variables)] identity: String,
        connector: Connector,
        retry_frequency: u64,
        max_retry_frequency: u64,
        endpoint_retry_frequency: u64,
    ) -> Result<PeerManager, PeerManagerError> {
        debug!(
            "Starting peer manager with identity={}, retry_interval={}s, max_retry_attempts={} \
            strict_ref_counts={}, retry_frequency={}, max_retry_frequency={}, and \
            endpoint_retry_frequency={}",
            identity,
            retry_interval,
            max_retry_attempts,
            strict_ref_counts,
            retry_frequency,
            max_retry_frequency,
            endpoint_retry_frequency,
        );

        let (sender, recv) = channel();

        let peer_remover = PeerRemover {
            sender: sender.clone(),
        };

        let subscriber_id = connector.subscribe(sender.clone()).map_err(|err| {
            PeerManagerError::StartUpError(format!(
                "Unable to subscribe to connection manager notifications: {}",
                err
            ))
        })?;

        debug!(
            "Starting peer manager pacemaker with interval of {}s",
            retry_interval
        );

        let pacemaker = pacemaker::Pacemaker::builder()
            .with_interval(retry_interval)
            .with_sender(sender.clone())
            .with_message_factory(|| PeerManagerMessage::RetryPending)
            .start()
            .map_err(|err| PeerManagerError::StartUpError(err.to_string()))?;

        let pacemaker_shutdown_signaler = pacemaker.shutdown_signaler();

        let join_handle = thread::Builder::new()
            .name("Peer Manager".into())
            .spawn(move || {
                let mut peers = PeerMap::new(retry_frequency);
                // a map of identities to unreferenced peers.
                // and a list of endpoints that should be turned into peers
                let mut unreferenced_peers = UnreferencedPeerState::new(endpoint_retry_frequency);
                let mut ref_map = RefMap::new();
                let mut subscribers = SubscriberMap::new();
                loop {
                    match recv.recv() {
                        Ok(PeerManagerMessage::Shutdown) => break,
                        Ok(PeerManagerMessage::Request(request)) => {
                            handle_request(
                                request,
                                connector.clone(),
                                &mut unreferenced_peers,
                                &mut peers,
                                &peer_remover,
                                &mut ref_map,
                                &mut subscribers,
                                strict_ref_counts,
                            );
                        }
                        Ok(PeerManagerMessage::Subscribe(sender)) => {
                            // drop subscriber id because it will not be sent back
                            subscribers.add_subscriber(Box::new(move |notification| {
                                sender.send(notification).map_err(Box::from)
                            }));
                        }
                        Ok(PeerManagerMessage::InternalNotification(notification)) => {
                            handle_notifications(
                                notification,
                                &mut unreferenced_peers,
                                &mut peers,
                                connector.clone(),
                                &mut subscribers,
                                max_retry_attempts,
                                &mut ref_map,
                                retry_frequency,
                            )
                        }
                        Ok(PeerManagerMessage::RetryPending) => retry_pending(
                            &mut peers,
                            connector.clone(),
                            &mut unreferenced_peers,
                            max_retry_frequency,
                        ),
                        Err(_) => {
                            warn!("All senders have disconnected");
                            break;
                        }
                    }
                }

                if let Err(err) = connector.unsubscribe(subscriber_id) {
                    error!(
                        "Unable to unsubscribe from connection manager notifications: {}",
                        err
                    );
                }

                debug!("Shutting down peer manager pacemaker...");
                pacemaker.await_shutdown();
                debug!("Shutting down peer manager pacemaker (complete)");
            })
            .map_err(|err| {
                PeerManagerError::StartUpError(format!(
                    "Unable to start PeerManager thread {}",
                    err
                ))
            })?;

        Ok(PeerManager {
            join_handle,
            sender,
            pacemaker_shutdown_signaler,
        })
    }
}

impl ShutdownHandle for PeerManager {
    fn signal_shutdown(&mut self) {
        self.pacemaker_shutdown_signaler.shutdown();
        if self.sender.send(PeerManagerMessage::Shutdown).is_err() {
            warn!("PeerManager is no longer running");
        }
    }

    fn wait_for_shutdown(self) -> Result<(), InternalError> {
        debug!("Shutting down peer manager...");
        self.join_handle.join().map_err(|err| {
            InternalError::with_message(format!(
                "Peer manager thread did not shutdown correctly: {:?}",
                err
            ))
        })?;
        debug!("Shutting down peer manager (complete)");
        Ok(())
    }
}

// Allow clippy errors for too_many_arguments. The arguments are required
// to avoid needing a lock in the PeerManager.
#[allow(clippy::too_many_arguments)]
fn handle_request(
    request: PeerManagerRequest,
    connector: Connector,
    unreferenced_peers: &mut UnreferencedPeerState,
    peers: &mut PeerMap,
    peer_remover: &PeerRemover,
    ref_map: &mut RefMap<PeerTokenPair>,
    subscribers: &mut SubscriberMap,
    strict_ref_counts: bool,
) {
    match request {
        PeerManagerRequest::AddPeer {
            peer_id,
            endpoints,
            required_local_auth,
            sender,
        } => {
            if sender
                .send(add_peer(
                    peer_id,
                    endpoints,
                    connector,
                    unreferenced_peers,
                    peers,
                    peer_remover,
                    ref_map,
                    subscribers,
                    required_local_auth,
                ))
                .is_err()
            {
                warn!("Connector dropped before receiving result of adding peer");
            }
        }
        PeerManagerRequest::AddUnidentified {
            endpoint,
            local_authorization,
            sender,
        } => {
            if sender
                .send(Ok(add_unidentified(
                    endpoint,
                    connector,
                    unreferenced_peers,
                    peer_remover,
                    peers,
                    ref_map,
                    local_authorization,
                )))
                .is_err()
            {
                warn!("Connector dropped before receiving result of adding unidentified peer");
            }
        }
        PeerManagerRequest::RemovePeer { peer_id, sender } => {
            if sender
                .send(remove_peer(
                    peer_id,
                    connector,
                    unreferenced_peers,
                    peers,
                    ref_map,
                    strict_ref_counts,
                ))
                .is_err()
            {
                warn!("Connector dropped before receiving result of removing peer");
            }
        }
        PeerManagerRequest::RemovePeerByEndpoint {
            endpoint,
            connection_id,
            sender,
        } => {
            if sender
                .send(remove_peer_by_endpoint(
                    endpoint,
                    connection_id,
                    connector,
                    peers,
                    ref_map,
                    strict_ref_counts,
                ))
                .is_err()
            {
                warn!("Connector dropped before receiving result of removing peer");
            }
        }
        PeerManagerRequest::ListPeers { sender } => {
            if sender.send(Ok(peers.peer_ids())).is_err() {
                warn!("Connector dropped before receiving result of list peers");
            }
        }

        PeerManagerRequest::ListUnreferencedPeers { sender } => {
            let peer_ids = unreferenced_peers
                .peers
                .keys()
                .map(|s| s.to_owned())
                .collect();
            if sender.send(Ok(peer_ids)).is_err() {
                warn!("Connector dropped before receiving result of list unreferenced peers");
            }
        }
        PeerManagerRequest::ConnectionIds { sender } => {
            if sender.send(Ok(peers.connection_ids())).is_err() {
                warn!("Connector dropped before receiving result of connection IDs");
            }
        }
        PeerManagerRequest::GetConnectionId { peer_id, sender } => {
            let connection_id = peers
                .get_by_peer_id(&peer_id)
                .map(|meta| meta.connection_id.clone())
                .or_else(|| {
                    unreferenced_peers
                        .peers
                        .get(&peer_id)
                        .map(|meta| meta.connection_id.clone())
                });

            if sender.send(Ok(connection_id)).is_err() {
                warn!("Connector dropped before receiving result of getting connection ID");
            }
        }
        PeerManagerRequest::GetPeerId {
            connection_id,
            sender,
        } => {
            let peer_id = peers
                .get_by_connection_id(&connection_id)
                .map(|meta| PeerTokenPair::new(meta.id.clone(), meta.required_local_auth.clone()))
                .or_else(|| {
                    unreferenced_peers
                        .get_by_connection_id(&connection_id)
                        .map(|(peer_id, _)| peer_id)
                });

            if sender.send(Ok(peer_id)).is_err() {
                warn!("Connector dropped before receiving result of getting peer ID");
            }
        }
        PeerManagerRequest::Subscribe { sender, callback } => {
            let subscriber_id = subscribers.add_subscriber(callback);
            if sender.send(Ok(subscriber_id)).is_err() {
                warn!("connector dropped before receiving result of remove connection");
            }
        }
        PeerManagerRequest::Unsubscribe {
            sender,
            subscriber_id,
        } => {
            subscribers.remove_subscriber(subscriber_id);
            if sender.send(Ok(())).is_err() {
                warn!("connector dropped before receiving result of remove connection");
            }
        }
    };
}

// Allow clippy errors for too_many_arguments. The arguments are required
// to avoid needing a lock in the PeerManager.
#[allow(clippy::too_many_arguments)]
fn add_peer(
    peer_id: PeerAuthorizationToken,
    endpoints: Vec<String>,
    connector: Connector,
    unreferenced_peers: &mut UnreferencedPeerState,
    peers: &mut PeerMap,
    peer_remover: &PeerRemover,
    ref_map: &mut RefMap<PeerTokenPair>,
    subscribers: &mut SubscriberMap,
    required_local_auth: PeerAuthorizationToken,
) -> Result<PeerRef, PeerRefAddError> {
    let peer_token_pair = PeerTokenPair::new(peer_id.clone(), required_local_auth.clone());

    if check_for_duplicate_endpoint(&peer_id, &endpoints, peers) {
        return Err(PeerRefAddError::AddError(format!(
            "Peer {} contains endpoints that already belong to another peer using trust",
            peer_id
        )));
    }

    let new_ref_count = ref_map.add_ref(peer_token_pair.clone());

    // if this is not a new peer, return success
    if new_ref_count > 1 {
        if let Some(mut peer_metadata) = peers.get_by_peer_id(&peer_token_pair).cloned() {
            if peer_metadata.endpoints.len() == 1 && endpoints.len() > 1 {
                // this should always be true
                if let Some(endpoint) = peer_metadata.endpoints.get(0) {
                    // if peer was added by endpoint, its peer metadata should be updated to
                    // include the full list of endpoints in this request
                    if unreferenced_peers
                        .requested_endpoints
                        .contains_key(endpoint)
                        && endpoints.contains(endpoint)
                    {
                        info!(
                            "Updating peer {} to include endpoints {:?}",
                            peer_id, endpoints
                        );
                        peer_metadata.endpoints = endpoints;
                        peers.update_peer(peer_metadata.clone()).map_err(|err| {
                            PeerRefAddError::AddError(format!(
                                "Unable to update peer {}: {}",
                                peer_id, err
                            ))
                        })?
                    } else {
                        // remove ref we just added
                        if let Err(err) = ref_map.remove_ref(&peer_token_pair) {
                            error!(
                                "Unable to remove ref that was just added for peer {}: {}",
                                peer_id, err
                            );
                        };

                        return Err(PeerRefAddError::AddError(format!(
                            "Mismatch betwen existing and requested peer endpoints: {:?} does not \
                            contain {}",
                            endpoints, endpoint
                        )));
                    }
                } else {
                    return Err(PeerRefAddError::AddError(format!(
                        "Peer {} does not have any endpoints",
                        peer_id
                    )));
                }
            }

            // notify subscribers this peer is connected
            if peer_metadata.status == PeerStatus::Connected {
                // Update peer for new state
                let notification = PeerManagerNotification::Connected {
                    peer: peer_token_pair.clone(),
                };
                subscribers.broadcast(notification);
            }

            let peer_ref = PeerRef::new(peer_token_pair, peer_remover.clone());
            return Ok(peer_ref);
        } else {
            return Err(PeerRefAddError::AddError(format!(
                "A reference exists for peer {} but missing peer metadata",
                peer_id
            )));
        }
    };

    // if it is an unreferenced peer, promote it to a fully-referenced peer
    if let Some(UnreferencedPeer {
        connection_id,
        endpoint,
        old_connection_ids,
        ..
    }) = unreferenced_peers.peers.remove(&peer_token_pair)
    {
        debug!("Updating unreferenced peer to full peer {}", peer_id);
        peers.insert(
            peer_id,
            connection_id,
            endpoints,
            endpoint,
            PeerStatus::Connected,
            required_local_auth,
            old_connection_ids,
        );

        // Update peer for new state
        let notification = PeerManagerNotification::Connected {
            peer: peer_token_pair.clone(),
        };
        subscribers.broadcast(notification);

        let peer_ref = PeerRef::new(peer_token_pair.clone(), peer_remover.clone());
        return Ok(peer_ref);
    }

    info!("Attempting to peer with {}", peer_id);
    let connection_id = format!("{}", Uuid::new_v4());

    let mut active_endpoint = match endpoints.get(0) {
        Some(endpoint) => endpoint.to_string(),
        None => {
            // remove ref we just added
            if let Err(err) = ref_map.remove_ref(&peer_token_pair) {
                error!(
                    "Unable to remove ref that was just added for peer {}: {}",
                    peer_id, err
                );
            };
            return Err(PeerRefAddError::AddError(format!(
                "No endpoints provided for peer {}",
                peer_id
            )));
        }
    };

    for endpoint in endpoints.iter() {
        match connector.request_connection(
            endpoint,
            &connection_id,
            Some(peer_id.clone().into()),
            Some(required_local_auth.clone().into()),
        ) {
            Ok(()) => {
                active_endpoint = endpoint.to_string();
                break;
            }
            // If the request_connection errored we will retry in the future
            Err(err) => {
                log_connect_request_err(err, &peer_id, endpoint);
            }
        }
    }

    peers.insert(
        peer_id,
        connection_id,
        endpoints.to_vec(),
        active_endpoint,
        PeerStatus::Pending,
        required_local_auth,
        vec![],
    );
    let peer_ref = PeerRef::new(peer_token_pair, peer_remover.clone());
    Ok(peer_ref)
}

// Request a connection, the resulting connection will be treated as an InboundConnection
fn add_unidentified(
    endpoint: String,
    connector: Connector,
    unreferenced_peers: &mut UnreferencedPeerState,
    peer_remover: &PeerRemover,
    peers: &PeerMap,
    ref_map: &mut RefMap<PeerTokenPair>,
    local_authorization: PeerAuthorizationToken,
) -> EndpointPeerRef {
    info!("Attempting to peer with peer by endpoint {}", endpoint);
    if let Some(peer_metadatas) = peers.get_peer_from_endpoint(&endpoint) {
        for peer_metadata in peer_metadatas {
            // need to verify that the existing peer has the correct local authorization
            if peer_metadata.required_local_auth == local_authorization {
                let peer_token_pair = PeerTokenPair::new(
                    peer_metadata.id.clone(),
                    peer_metadata.required_local_auth.clone(),
                );
                // if there is peer in the peer_map, there is reference in the ref map
                ref_map.add_ref(peer_token_pair);
                return EndpointPeerRef::new(
                    endpoint,
                    peer_metadata.connection_id,
                    peer_remover.clone(),
                );
            }
        }
    }

    let connection_id = format!("{}", Uuid::new_v4());
    match connector.request_connection(
        &endpoint,
        &connection_id,
        None,
        Some(local_authorization.clone().into()),
    ) {
        Ok(()) => (),
        Err(err) => {
            warn!("Unable to peer with peer at {}: {}", endpoint, err);
        }
    };
    unreferenced_peers.requested_endpoints.insert(
        endpoint.to_string(),
        RequestedEndpoint {
            endpoint: endpoint.to_string(),
            local_authorization,
        },
    );
    EndpointPeerRef::new(endpoint, connection_id, peer_remover.clone())
}

fn remove_peer(
    peer_id: PeerTokenPair,
    connector: Connector,
    unreferenced_peers: &mut UnreferencedPeerState,
    peers: &mut PeerMap,
    ref_map: &mut RefMap<PeerTokenPair>,
    strict_ref_counts: bool,
) -> Result<(), PeerRefRemoveError> {
    debug!("Removing peer: {}", peer_id);

    // remove from the unreferenced peers, if it is there.
    unreferenced_peers.peers.remove(&peer_id);

    // remove the reference
    let removed_peer = match ref_map.remove_ref(&peer_id) {
        Ok(removed_peer) => removed_peer,
        Err(err) => {
            if strict_ref_counts {
                panic!(
                    "Trying to remove a reference that does not exist: {}",
                    peer_id
                );
            } else {
                return Err(PeerRefRemoveError::Remove(format!(
                    "Failed to remove ref for peer {} from ref map: {}",
                    peer_id, err
                )));
            }
        }
    };

    if removed_peer.is_some() {
        let peer_metadata = peers.remove(&peer_id).ok_or_else(|| {
            PeerRefRemoveError::Remove(format!(
                "Peer {} has already been removed from the peer map",
                peer_id
            ))
        })?;

        // If the peer is pending there is no connection to remove
        if peer_metadata.status == PeerStatus::Pending {
            return Ok(());
        }
        match connector
            .remove_connection(&peer_metadata.active_endpoint, &peer_metadata.connection_id)
        {
            Ok(Some(_)) => {
                debug!(
                    "Peer {} has been removed and connection {} has been closed",
                    peer_id, peer_metadata.active_endpoint
                );
                Ok(())
            }
            Ok(None) => Err(PeerRefRemoveError::Remove(format!(
                "The connection for peer {}'s active endpoint ({}) has already been removed",
                peer_id, peer_metadata.active_endpoint
            ))),
            Err(err) => Err(PeerRefRemoveError::Remove(format!("{}", err))),
        }
    } else {
        // if the peer has not been fully removed, return OK
        Ok(())
    }
}

fn remove_peer_by_endpoint(
    endpoint: String,
    connection_id: String,
    connector: Connector,
    peers: &mut PeerMap,
    ref_map: &mut RefMap<PeerTokenPair>,
    strict_ref_counts: bool,
) -> Result<(), PeerRefRemoveError> {
    let peer_metadata = match peers.get_by_connection_id(&connection_id) {
        Some(peer_metadata) => peer_metadata,
        None => {
            return Err(PeerRefRemoveError::Remove(format!(
                "Peer with endpoint {} has already been removed from the peer map",
                endpoint
            )))
        }
    };

    let peer_token_pair = PeerTokenPair::new(
        peer_metadata.id.clone(),
        peer_metadata.required_local_auth.clone(),
    );

    debug!(
        "Removing peer {} by endpoint: {}",
        peer_token_pair, endpoint
    );
    // remove the reference
    let removed_peer = match ref_map.remove_ref(&peer_token_pair) {
        Ok(removed_peer) => removed_peer,
        Err(err) => {
            if strict_ref_counts {
                panic!(
                    "Trying to remove a reference that does not exist: {}",
                    peer_token_pair
                );
            } else {
                return Err(PeerRefRemoveError::Remove(format!(
                    "Failed to remove ref for peer {} from ref map: {}",
                    peer_token_pair, err
                )));
            }
        }
    };
    if let Some(removed_peer) = removed_peer {
        let peer_metadata = peers.remove(&removed_peer).ok_or_else(|| {
            PeerRefRemoveError::Remove(format!(
                "Peer with endpoint {} has already been removed from the peer map",
                endpoint
            ))
        })?;

        // If the peer is pending there is no connection to remove
        if peer_metadata.status == PeerStatus::Pending {
            return Ok(());
        }

        match connector
            .remove_connection(&peer_metadata.active_endpoint, &peer_metadata.connection_id)
        {
            Ok(Some(_)) => {
                debug!(
                    "Peer {} has been removed and connection {} has been closed",
                    peer_token_pair, peer_metadata.active_endpoint
                );
                Ok(())
            }
            Ok(None) => Err(PeerRefRemoveError::Remove(format!(
                "The connection for peer {}'s active endpoint ({}) has already been removed",
                peer_token_pair, peer_metadata.active_endpoint
            ))),
            Err(err) => Err(PeerRefRemoveError::Remove(format!("{}", err))),
        }
    } else {
        // if the peer has not been fully removed, return OK
        Ok(())
    }
}

// Allow clippy errors for too_many_arguments. The arguments are required
// to avoid needing a lock in the PeerManager.
#[allow(clippy::too_many_arguments)]
fn handle_notifications(
    notification: ConnectionManagerNotification,
    unreferenced_peers: &mut UnreferencedPeerState,
    peers: &mut PeerMap,
    connector: Connector,
    subscribers: &mut SubscriberMap,
    max_retry_attempts: u64,
    ref_map: &mut RefMap<PeerTokenPair>,
    retry_frequency: u64,
) {
    match notification {
        // If a connection has disconnected, forward notification to subscribers
        ConnectionManagerNotification::Disconnected {
            endpoint,
            identity,
            connection_id,
        } => handle_disconnection(
            endpoint,
            PeerAuthorizationToken::from(identity),
            connection_id,
            unreferenced_peers,
            peers,
            connector,
            subscribers,
        ),
        ConnectionManagerNotification::NonFatalConnectionError {
            endpoint,
            attempts,
            connection_id,
            ..
        } => {
            // Check if the disconnected peer has reached the retry limit, if so try to find a
            // different endpoint that can be connected to
            if let Some(mut peer_metadata) = peers.get_by_connection_id(&connection_id).cloned() {
                info!(
                    "{} reconnection attempts have been made to peer {}",
                    attempts, peer_metadata.id
                );
                if attempts >= max_retry_attempts {
                    if endpoint != peer_metadata.active_endpoint {
                        warn!(
                            "Received non fatal connection notification for peer {} with \
                            different endpoint {}",
                            peer_metadata.id, endpoint
                        );
                        return;
                    };
                    info!(
                        "Attempting to find available endpoint for {}",
                        peer_metadata.id
                    );
                    for endpoint in peer_metadata.endpoints.iter() {
                        // do not retry the connection that is currently failing
                        if endpoint == &peer_metadata.active_endpoint {
                            continue;
                        }
                        match connector.request_connection(
                            endpoint,
                            &peer_metadata.connection_id,
                            Some(peer_metadata.id.clone().into()),
                            Some(peer_metadata.required_local_auth.clone().into()),
                        ) {
                            Ok(()) => break,
                            Err(err) => {
                                log_connect_request_err(err, &peer_metadata.id, endpoint);
                            }
                        }
                    }
                }

                peer_metadata.status = PeerStatus::Disconnected {
                    retry_attempts: attempts,
                };

                if let Err(err) = peers.update_peer(peer_metadata) {
                    error!("Unable to update peer: {}", err);
                }
            }
        }
        ConnectionManagerNotification::InboundConnection {
            endpoint,
            connection_id,
            identity,
            local_identity,
        } => handle_inbound_connection(
            endpoint,
            PeerAuthorizationToken::from(identity),
            connection_id,
            PeerAuthorizationToken::from(local_identity),
            unreferenced_peers,
            peers,
            connector,
            subscribers,
            retry_frequency,
        ),
        ConnectionManagerNotification::Connected {
            endpoint,
            identity,
            local_identity,
            connection_id,
        } => handle_connected(
            endpoint,
            PeerAuthorizationToken::from(identity),
            connection_id,
            PeerAuthorizationToken::from(local_identity),
            unreferenced_peers,
            peers,
            connector,
            subscribers,
            ref_map,
            retry_frequency,
        ),
        ConnectionManagerNotification::FatalConnectionError {
            connection_id,
            error,
            ..
        } => handle_fatal_connection(
            connection_id,
            error.to_string(),
            peers,
            subscribers,
            max_retry_attempts,
        ),
    }
}

fn handle_disconnection(
    endpoint: String,
    identity: PeerAuthorizationToken,
    connection_id: String,
    unreferenced_peers: &mut UnreferencedPeerState,
    peers: &mut PeerMap,
    connector: Connector,
    subscribers: &mut SubscriberMap,
) {
    if let Some(mut peer_metadata) = peers.get_by_connection_id(&connection_id).cloned() {
        if endpoint != peer_metadata.active_endpoint {
            warn!(
                "Received disconnection notification for peer {} with \
                different endpoint {}",
                peer_metadata.id, endpoint
            );
            return;
        }

        let notification = PeerManagerNotification::Disconnected {
            peer: PeerTokenPair::new(
                peer_metadata.id.clone(),
                peer_metadata.required_local_auth.clone(),
            ),
        };
        info!("Peer {} is currently disconnected", peer_metadata.id);
        if peer_metadata.endpoints.contains(&endpoint) {
            // allow peer manager to retry connection to that endpoint until the retry max is
            // reached

            // set peer to disconnected
            peer_metadata.status = PeerStatus::Disconnected { retry_attempts: 1 };
            if let Err(err) = peers.update_peer(peer_metadata) {
                error!("Unable to update peer: {}", err);
            }
        } else {
            // the disconnected endpoint is an inbound connection. This connection should
            // be removed, peer set to pending and the endpoints in the peer metadata
            // should be tried
            if let Err(err) = connector
                .remove_connection(&peer_metadata.active_endpoint, &peer_metadata.connection_id)
            {
                error!("Unable to clean up old connection: {}", err);
            }

            info!("Attempting to find available endpoint for {}", identity);
            for endpoint in peer_metadata.endpoints.iter() {
                match connector.request_connection(
                    endpoint,
                    &peer_metadata.connection_id,
                    Some(identity.clone().into()),
                    Some(peer_metadata.required_local_auth.clone().into()),
                ) {
                    Ok(()) => break,
                    Err(err) => {
                        log_connect_request_err(err, &peer_metadata.id, endpoint);
                    }
                }
            }
            peer_metadata.status = PeerStatus::Pending;
            if let Err(err) = peers.update_peer(peer_metadata) {
                error!("Unable to update peer: {}", err);
            }
        }
        subscribers.broadcast(notification);
    } else {
        // check for unreferenced peer and remove if it has disconnected
        debug!("Removing disconnected peer: {}", identity);
        let unreferenced_peer = unreferenced_peers.get_by_connection_id(&connection_id);

        if let Some((id, unref_peer)) = unreferenced_peer {
            unreferenced_peers.peers.remove(&id);
            if let Err(err) =
                connector.remove_connection(&unref_peer.endpoint, &unref_peer.connection_id)
            {
                error!("Unable to clean up old connection: {}", err);
            }
        }
    }
}

// Allow clippy errors for too_many_arguments. The arguments are required
// to avoid needing a lock in the PeerManager.
#[allow(clippy::too_many_arguments)]
fn handle_inbound_connection(
    endpoint: String,
    identity: PeerAuthorizationToken,
    connection_id: String,
    local_authorization: PeerAuthorizationToken,
    unreferenced_peers: &mut UnreferencedPeerState,
    peers: &mut PeerMap,
    connector: Connector,
    subscribers: &mut SubscriberMap,
    retry_frequency: u64,
) {
    info!(
        "Received peer connection from {} (remote endpoint: {})",
        identity, endpoint
    );

    let peer_token_pair = PeerTokenPair::new(identity.clone(), local_authorization.clone());
    // If we got an inbound counnection for an existing peer, replace old connection with
    // this new one unless we are already connected.
    if let Some(mut peer_metadata) = peers.get_by_peer_id(&peer_token_pair).cloned() {
        match peer_metadata.status {
            PeerStatus::Disconnected { .. } => {
                info!(
                    "Adding inbound connection to Disconnected peer: {}",
                    peer_metadata.id
                );
            }
            PeerStatus::Pending => {
                info!(
                    "Adding inbound connection to Pending peer: {} ({})",
                    identity, connection_id
                );
            }
            PeerStatus::Connected => {
                // Compare identities, if local identity is greater, close incoming connection
                // otherwise, remove outbound connection and replace with inbound.
                if peer_metadata.required_local_auth > identity {
                    // if peer is already connected, remove the inbound connection
                    debug!(
                        "Removing inbound connection, already connected to {} ({})",
                        peer_metadata.id, connection_id
                    );
                    if let Err(err) = connector.remove_connection(&endpoint, &connection_id) {
                        error!("Unable to clean up old connection: {}", err);
                    }
                    return;
                } else {
                    info!(
                        "Replacing existing connection with inbound for peer {} ({})",
                        peer_metadata.id, connection_id
                    );
                }
            }
        }
        let old_endpoint = peer_metadata.active_endpoint;
        let old_connection_id = peer_metadata.connection_id;
        let starting_status = peer_metadata.status;
        peer_metadata.status = PeerStatus::Connected;
        peer_metadata.connection_id = connection_id.clone();
        // reset retry settings
        peer_metadata.retry_frequency = retry_frequency;
        peer_metadata.last_connection_attempt = Instant::now();

        let notification = PeerManagerNotification::Connected {
            peer: peer_token_pair.clone(),
        };

        peer_metadata.active_endpoint = endpoint;
        if let Err(err) = peers.update_peer(peer_metadata) {
            error!("Unable to update peer: {}", err);
        }

        subscribers.broadcast(notification);

        // if peer is pending there is no connection to remove
        if connection_id != old_connection_id && starting_status != PeerStatus::Pending {
            if let Err(err) = connector.remove_connection(&old_endpoint, &old_connection_id) {
                warn!("Unable to clean up old connection: {}", err);
            }
        }
    } else if let Some(unreferenced_peer) = unreferenced_peers.peers.get_mut(&peer_token_pair) {
        // Compare identities, if local identity is greater, close incoming connection
        // otherwise, remove outbound connection and replace with inbound.
        if unreferenced_peer.local_authorization > identity {
            // if peer is already connected, remove the inbound connection
            debug!(
                "Removing inbound connection, already connected to unreferenced peer {} ({})",
                peer_token_pair, connection_id
            );
            if let Err(err) = connector.remove_connection(&endpoint, &connection_id) {
                error!("Unable to clean up old connection: {}", err);
            }
        } else {
            info!(
                "Replacing existing connection with inbound for unreferenced peer {} ({})",
                peer_token_pair, connection_id
            );

            debug!(
                "Removing old peer connection for unreferenced peer {}: {}",
                peer_token_pair, unreferenced_peer.connection_id
            );
            if let Err(err) = connector.remove_connection(
                &unreferenced_peer.endpoint,
                &unreferenced_peer.connection_id,
            ) {
                error!("Unable to clean up old connection: {}", err);
            }

            let mut old_connection_ids = unreferenced_peer.old_connection_ids.to_vec();
            old_connection_ids.push(unreferenced_peer.connection_id.to_string());

            *unreferenced_peer = UnreferencedPeer {
                connection_id,
                endpoint,
                local_authorization,
                old_connection_ids,
            };
        }
    } else {
        debug!(
            "Add inbound unreferenced peer for {} ({})",
            peer_token_pair, connection_id
        );
        unreferenced_peers.peers.insert(
            peer_token_pair,
            UnreferencedPeer {
                connection_id,
                endpoint,
                local_authorization,
                old_connection_ids: vec![],
            },
        );
    }
}

// Allow clippy errors for too_many_arguments and cognitive_complexity. The arguments are required
// to avoid needing a lock in the PeerManager.
#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
fn handle_connected(
    endpoint: String,
    identity: PeerAuthorizationToken,
    connection_id: String,
    local_authorization: PeerAuthorizationToken,
    unreferenced_peers: &mut UnreferencedPeerState,
    peers: &mut PeerMap,
    connector: Connector,
    subscribers: &mut SubscriberMap,
    ref_map: &mut RefMap<PeerTokenPair>,
    retry_frequency: u64,
) {
    let peer_token_pair = PeerTokenPair::new(identity.clone(), local_authorization.clone());
    if let Some(mut peer_metadata) = peers.get_by_peer_id(&peer_token_pair).cloned() {
        match peer_metadata.status {
            PeerStatus::Pending => {
                info!(
                    "Pending peer {} connected via {}",
                    peer_metadata.id, endpoint
                );
            }
            PeerStatus::Disconnected { .. } => {
                info!(
                    "Disconnected peer {} connected via {}",
                    peer_metadata.id, endpoint
                );
            }
            PeerStatus::Connected => {
                // Compare identities, if remote identity is greater, remove outbound connection
                // otherwise replace inbound connection with outbound.
                if peer_metadata.required_local_auth < identity {
                    info!(
                        "Removing outbound connection, peer {} is already connected ({})",
                        peer_metadata.id, connection_id
                    );
                    // we are already connected on another connection, remove this connection
                    if endpoint != peer_metadata.active_endpoint {
                        if let Err(err) = connector.remove_connection(&endpoint, &connection_id) {
                            error!("Unable to clean up old connection: {}", err);
                        }
                    }
                    return;
                } else {
                    info!(
                        "Replacing existing connection with outbound for peer {} connected via \
                         {} ({})",
                        peer_metadata.id, endpoint, connection_id
                    );
                }
            }
        }

        // Update peer for new state
        let notification = PeerManagerNotification::Connected {
            peer: peer_token_pair.clone(),
        };

        let starting_status = peer_metadata.status;
        let old_endpoint = peer_metadata.active_endpoint;
        let old_connection_id = peer_metadata.connection_id;
        peer_metadata.active_endpoint = endpoint;
        peer_metadata.status = PeerStatus::Connected;
        peer_metadata.connection_id = connection_id.clone();
        // reset retry settings
        peer_metadata.retry_frequency = retry_frequency;
        peer_metadata.last_connection_attempt = Instant::now();

        if let Err(err) = peers.update_peer(peer_metadata) {
            error!("Unable to update peer: {}", err);
        }

        // remove old connection
        if connection_id != old_connection_id && starting_status != PeerStatus::Pending {
            if let Err(err) = connector.remove_connection(&old_endpoint, &old_connection_id) {
                error!("Unable to clean up old connection: {}", err);
            }
        }

        // notify subscribers we are connected
        subscribers.broadcast(notification);
    } else {
        // if this endpoint has been requested, add this connection to peers with the provided
        // endpoint
        if let Some(requested_endpoint) = unreferenced_peers.requested_endpoints.get(&endpoint) {
            let mut new_peer_endpoint = endpoint.to_string();
            let mut new_peer_connection_id = connection_id.clone();
            let mut old_connection_ids = vec![];
            if let Some(unreferenced_peer) = unreferenced_peers.peers.remove(&peer_token_pair) {
                if unreferenced_peer.local_authorization < identity {
                    info!(
                        "Removing outbound connection, peer {} is already connected via \
                            unreferenced ({})",
                        peer_token_pair, connection_id
                    );

                    new_peer_endpoint = unreferenced_peer.endpoint.to_string();
                    new_peer_connection_id = unreferenced_peer.connection_id.to_string();
                    old_connection_ids = unreferenced_peer.old_connection_ids.clone();
                    old_connection_ids.push(connection_id.clone());

                    // we are already connected on another connection, remove this connection
                    if endpoint != unreferenced_peer.endpoint {
                        if let Err(err) = connector.remove_connection(&endpoint, &connection_id) {
                            error!("Unable to clean up old connection: {}", err);
                        }
                    }
                } else {
                    info!(
                        "Replacing existing unreferenced connection with outbound for peer {} \
                            connected via {} ({})",
                        peer_token_pair, endpoint, connection_id
                    );

                    old_connection_ids.push(unreferenced_peer.connection_id.to_string());

                    if let Err(err) = connector.remove_connection(
                        &unreferenced_peer.endpoint,
                        &unreferenced_peer.connection_id,
                    ) {
                        error!("Unable to clean up old connection: {}", err);
                    }
                }
            }

            debug!(
                "Adding peer {} by endpoint {} ({})",
                peer_token_pair, endpoint, connection_id
            );
            ref_map.add_ref(peer_token_pair.clone());
            peers.insert(
                identity,
                new_peer_connection_id,
                vec![endpoint.to_string()],
                new_peer_endpoint,
                PeerStatus::Connected,
                requested_endpoint.local_authorization.clone(),
                old_connection_ids,
            );

            let notification = PeerManagerNotification::Connected {
                peer: peer_token_pair.clone(),
            };
            subscribers.broadcast(notification);
            return;
        }

        if let Some(unreferenced_peer) = unreferenced_peers.peers.get_mut(&peer_token_pair) {
            // Compare identities, if remote identity is greater, remove outbound connection
            // otherwise replace inbound connection with outbound.
            if unreferenced_peer.local_authorization < identity {
                // if peer is already connected, remove the outbound connection
                debug!(
                    "Removing outbound connection, already connected to unreferenced peer {} ({})",
                    peer_token_pair, connection_id
                );
                if let Err(err) = connector.remove_connection(&endpoint, &connection_id) {
                    error!("Unable to clean up old connection: {}", err);
                }
            } else {
                info!(
                    "Replacing existing connection with outbound for unreferenced peer {} ({})",
                    peer_token_pair, connection_id
                );

                debug!(
                    "Removing old peer connection for unreferenced peer {}: {}",
                    peer_token_pair, unreferenced_peer.connection_id
                );
                if let Err(err) = connector.remove_connection(
                    &unreferenced_peer.endpoint,
                    &unreferenced_peer.connection_id,
                ) {
                    error!("Unable to clean up old connection: {}", err);
                }

                let mut old_connection_ids = unreferenced_peer.old_connection_ids.to_vec();
                old_connection_ids.push(unreferenced_peer.connection_id.to_string());

                *unreferenced_peer = UnreferencedPeer {
                    connection_id,
                    endpoint,
                    local_authorization,
                    old_connection_ids,
                };
            }
        } else {
            debug!(
                "Adding outbound unreferenced peer {} by endpoint {} ({})",
                peer_token_pair, endpoint, connection_id
            );
            unreferenced_peers.peers.insert(
                peer_token_pair,
                UnreferencedPeer {
                    connection_id,
                    endpoint,
                    local_authorization,
                    old_connection_ids: vec![],
                },
            );
        }
    }
}

fn handle_fatal_connection(
    connection_id: String,
    error: String,
    peers: &mut PeerMap,
    subscribers: &mut SubscriberMap,
    max_retry_frequency: u64,
) {
    if let Some(mut peer_metadata) = peers.get_by_connection_id(&connection_id).cloned() {
        warn!(
            "Peer {} encountered a fatal connection error: {}",
            peer_metadata.id, error
        );

        // Tell subscribers this peer is disconnected
        let notification = PeerManagerNotification::Disconnected {
            peer: PeerTokenPair::new(
                peer_metadata.id.clone(),
                peer_metadata.required_local_auth.clone(),
            ),
        };

        // reset retry settings
        peer_metadata.retry_frequency = min(peer_metadata.retry_frequency * 2, max_retry_frequency);
        peer_metadata.last_connection_attempt = Instant::now();

        // set peer to pending so its endpoints will be retried in the future
        peer_metadata.status = PeerStatus::Pending;
        if let Err(err) = peers.update_peer(peer_metadata) {
            error!("Unable to update peer: {}", err);
        }

        subscribers.broadcast(notification);
    }
}

// If a pending peer's retry_frequency has elapsed, retry their endpoints. If successful,
// their active endpoint will be updated. The retry_frequency will be increased and
// and last_connection_attempt reset.
fn retry_pending(
    peers: &mut PeerMap,
    connector: Connector,
    unreferenced_peers: &mut UnreferencedPeerState,
    max_retry_frequency: u64,
) {
    let mut to_retry = Vec::new();
    for (_, peer) in peers.get_pending() {
        if peer.last_connection_attempt.elapsed().as_secs() > peer.retry_frequency {
            to_retry.push(peer.clone());
        }
    }

    for mut peer_metadata in to_retry {
        debug!("Attempting to peer with pending peer {}", peer_metadata.id);
        for endpoint in peer_metadata.endpoints.iter() {
            match connector.request_connection(
                endpoint,
                &peer_metadata.connection_id,
                Some(peer_metadata.id.clone().into()),
                Some(peer_metadata.required_local_auth.clone().into()),
            ) {
                Ok(()) => peer_metadata.active_endpoint = endpoint.to_string(),
                // If request_connection errored we will retry in the future
                Err(err) => {
                    log_connect_request_err(err, &peer_metadata.id, endpoint);
                }
            }
        }

        peer_metadata.retry_frequency = min(peer_metadata.retry_frequency * 2, max_retry_frequency);
        peer_metadata.last_connection_attempt = Instant::now();
        if let Err(err) = peers.update_peer(peer_metadata) {
            error!("Unable to update peer: {}", err);
        }
    }

    if unreferenced_peers
        .last_connection_attempt
        .elapsed()
        .as_secs()
        > unreferenced_peers.retry_frequency
    {
        for (endpoint, requested_endpoint) in unreferenced_peers.requested_endpoints.iter() {
            if peers.contains_endpoint(&requested_endpoint.endpoint) {
                continue;
            }
            info!("Attempting to peer with peer by {}", endpoint);
            let connection_id = format!("{}", Uuid::new_v4());
            match connector.request_connection(
                endpoint,
                &connection_id,
                None,
                Some(requested_endpoint.local_authorization.clone().into()),
            ) {
                Ok(()) => (),
                // If request_connection errored we will retry in the future
                Err(err) => match err {
                    ConnectionManagerError::ConnectionCreationError {
                        context,
                        error_kind: None,
                    } => {
                        info!(
                            "Unable to request connection for peer endpoint {}: {}",
                            endpoint, context
                        );
                    }
                    ConnectionManagerError::ConnectionCreationError {
                        context,
                        error_kind: Some(err_kind),
                    } => match err_kind {
                        ErrorKind::ConnectionRefused => info!(
                            "Received connection refused while attempting to establish a \
                                        connection to peer at endpoint {}",
                            endpoint
                        ),
                        _ => info!(
                            "Unable to request connection for peer at {}: {}",
                            endpoint, context,
                        ),
                    },
                    _ => info!(
                        "Unable to request connection for peer at endpoint {}: {}",
                        endpoint,
                        err.to_string()
                    ),
                },
            }
        }

        unreferenced_peers.last_connection_attempt = Instant::now();
    }
}

fn log_connect_request_err(
    err: ConnectionManagerError,
    peer_id: &PeerAuthorizationToken,
    endpoint: &str,
) {
    match err {
        ConnectionManagerError::ConnectionCreationError {
            context,
            error_kind: None,
        } => {
            info!(
                "Unable to request connection for peer {}: {}",
                peer_id, context
            );
        }
        ConnectionManagerError::ConnectionCreationError {
            context,
            error_kind: Some(err_kind),
        } => match err_kind {
            ErrorKind::ConnectionRefused => info!(
                "Received connection refused while attempting to establish a \
                        connection to peer {}: endpoint {}",
                peer_id, endpoint
            ),
            _ => info!(
                "Unable to request connection for peer {}: {}",
                peer_id, context
            ),
        },
        _ => info!(
            "Unable to request connection for peer {}: {}",
            peer_id,
            err.to_string()
        ),
    }
}

/// Check to make sure if multiple peer IDs point to the same endpoint, they are not diffferent
/// Trust peer token. There can only be one Trust peer per endpoint, but multiple Challenge peers
/// are okay.
fn check_for_duplicate_endpoint(
    peer_id: &PeerAuthorizationToken,
    endpoints: &[String],
    peer_map: &PeerMap,
) -> bool {
    if matches!(peer_id, PeerAuthorizationToken::Challenge { .. }) {
        return false;
    }

    for endpoint in endpoints {
        if let Some(peers) = peer_map.get_peer_from_endpoint(endpoint) {
            for peer_meta in peers {
                if matches!(peer_meta.id, PeerAuthorizationToken::Trust { .. })
                    && &peer_meta.id != peer_id
                {
                    return true;
                }
            }
        }
    }

    false
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::time::Duration;

    use protobuf::Message;

    use crate::mesh::Mesh;
    use crate::network::auth::ConnectionAuthorizationType;
    use crate::network::connection_manager::{
        AuthorizationResult, Authorizer, AuthorizerError, ConnectionManager,
    };
    use crate::protos::network::{NetworkMessage, NetworkMessageType};
    use crate::threading::lifecycle::ShutdownHandle;
    use crate::transport::inproc::InprocTransport;
    use crate::transport::raw::RawTransport;
    use crate::transport::{Connection, Transport};

    // Test that a call to add_peer_ref returns the correct PeerRef
    //
    // 1. add test_peer
    // 2. verify that the returned PeerRef contains the test_peer id
    // 3. verify the the a Connected notification is received
    #[test]
    fn test_peer_manager_add_peer() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();
        let (tx, notification_rx): (
            Sender<PeerManagerNotification>,
            mpsc::Receiver<PeerManagerNotification>,
        ) = channel();
        peer_connector
            .subscribe_sender(tx)
            .expect("Unable to get subscriber");
        let peer_ref = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        // timeout after 60 seconds
        let timeout = Duration::from_secs(60);
        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that a call to add_peer_ref, where the peer being added is a different trust peer id
    // with an endpoint that already belongs to another trust peer id is rejected.
    //
    // 1. add test_peer
    // 2. verify that the returned PeerRef contains the test_peer id
    // 3. verify the the a Connected notification is received
    // 4. try to add different_peer with the same endpoint as test_peer and verify that an error
    //    is returned
    #[test]
    fn test_peer_manager_add_peer_duplicate_endpoint() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();
        let (tx, notification_rx): (
            Sender<PeerManagerNotification>,
            mpsc::Receiver<PeerManagerNotification>,
        ) = channel();
        peer_connector
            .subscribe_sender(tx)
            .expect("Unable to get subscriber");
        let peer_ref = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        // timeout after 60 seconds
        let timeout = Duration::from_secs(60);
        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        if peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("different_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .is_ok()
        {
            panic!(
                "Should not have been able to add a different trust peer with duplicate \
                     endpoint"
            )
        }

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that a call to add_peer_ref with a peer with multiple endpoints is successful, even if
    // the first endpoint is not available
    //
    // 1. add test_peer with two endpoints. The first endpoint will fail and cause the peer
    //    manager to try the second
    // 2. verify that the returned PeerRef contains the test_peer id
    // 3. verify the the a Connected notification is received
    #[test]
    fn test_peer_manager_add_peer_endpoints() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();
        let (tx, notification_rx): (
            Sender<PeerManagerNotification>,
            mpsc::Receiver<PeerManagerNotification>,
        ) = channel();
        peer_connector
            .subscribe_sender(tx)
            .expect("Unable to get subscriber");
        let peer_ref = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec![
                    "inproc://bad_endpoint".to_string(),
                    "inproc://test".to_string(),
                ],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        // timeout after 60 seconds
        let timeout = Duration::from_secs(60);
        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that the same peer can be added multiple times.
    //
    // 1. add test_peer
    // 2. verify the the a Connected notification is received
    // 3. add the same peer, and see it is successful
    #[test]
    fn test_peer_manager_add_peer_multiple_times() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();
        let (tx, notification_rx): (
            Sender<PeerManagerNotification>,
            mpsc::Receiver<PeerManagerNotification>,
        ) = channel();
        peer_connector
            .subscribe_sender(tx)
            .expect("Unable to get subscriber");
        let peer_ref = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        // timeout after 60 seconds
        let timeout = Duration::from_secs(60);
        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        let peer_ref = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that list_peer returns the correct list of peers
    //
    // 1. add test_peer
    // 2. verify the the a Connected notification is received
    // 3. add next_peer
    // 4. verify the the a Connected notification is received
    // 5. call list_peers
    // 6. verify that the sorted list of peers contains both test_peer and next_peer
    #[test]
    fn test_peer_manager_list_peer() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut listener = transport.listen("inproc://test_2").unwrap();
        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new_multiple(&[
                "test_peer",
                "next_peer",
            ])))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();
        let (tx, notification_rx): (
            Sender<PeerManagerNotification>,
            mpsc::Receiver<PeerManagerNotification>,
        ) = channel();
        peer_connector
            .subscribe_sender(tx)
            .expect("Unable to get subscriber");
        let peer_ref_1 = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref_1.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        // timeout after 60 seconds
        let timeout = Duration::from_secs(60);
        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        let peer_ref_2 = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("next_peer"),
                vec!["inproc://test_2".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref_2.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("next_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("next_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        let mut peer_list = peer_connector
            .list_peers()
            .expect("Unable to get peer list");

        peer_list.sort();

        assert_eq!(
            peer_list,
            vec![
                PeerAuthorizationToken::from_peer_id("next_peer"),
                PeerAuthorizationToken::from_peer_id("test_peer")
            ]
        );

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that list_peer returns the correct list of connection IDs
    //
    // 1. add test_peer
    // 2. add next_peer
    // 3. call connection_ids
    // 4. verify that the sorted map contains both test_peer and next_peer
    #[test]
    fn test_peer_manager_connection_ids() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut listener = transport.listen("inproc://test_2").unwrap();
        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new_multiple(&[
                "test_peer",
                "next_peer",
            ])))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();
        let (tx, notification_rx): (
            Sender<PeerManagerNotification>,
            mpsc::Receiver<PeerManagerNotification>,
        ) = channel();
        peer_connector
            .subscribe_sender(tx)
            .expect("Unable to get subscriber");
        let peer_ref_1 = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref_1.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        // timeout after 60 seconds
        let timeout = Duration::from_secs(60);
        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        let peer_ref_2 = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("next_peer"),
                vec!["inproc://test_2".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref_2.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("next_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("next_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        let peers = peer_connector
            .connection_ids()
            .expect("Unable to get peer list");

        assert!(peers
            .get_by_key(&PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("next_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            ))
            .is_some());

        assert!(peers
            .get_by_key(&PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            ))
            .is_some());

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that when a PeerRef is dropped, a remove peer request is properly sent and the peer
    // is removed
    //
    // 1. add test_peer
    // 2. call list peers
    // 3. verify that the peer list contains test_peer
    // 4. drop the PeerRef
    // 5. call list peers
    // 6. verify that the new peer list is empty
    #[test]
    fn test_peer_manager_drop_peer_ref() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();

        {
            let (tx, notification_rx): (
                Sender<PeerManagerNotification>,
                mpsc::Receiver<PeerManagerNotification>,
            ) = channel();
            peer_connector
                .subscribe_sender(tx)
                .expect("Unable to get subscriber");
            let peer_ref = peer_connector
                .add_peer_ref(
                    PeerAuthorizationToken::from_peer_id("test_peer"),
                    vec!["inproc://test".to_string()],
                    PeerAuthorizationToken::from_peer_id("my_id"),
                )
                .expect("Unable to add peer");

            assert_eq!(
                peer_ref.peer_id(),
                &PeerTokenPair::new(
                    PeerAuthorizationToken::from_peer_id("test_peer"),
                    PeerAuthorizationToken::from_peer_id("my_id"),
                )
            );

            // timeout after 60 seconds
            let timeout = Duration::from_secs(60);
            let notification = notification_rx
                .recv_timeout(timeout)
                .expect("Unable to get new notifications");
            assert!(
                notification
                    == PeerManagerNotification::Connected {
                        peer: PeerTokenPair::new(
                            PeerAuthorizationToken::from_peer_id("test_peer"),
                            PeerAuthorizationToken::from_peer_id("my_id"),
                        )
                    }
            );

            let peer_list = peer_connector
                .list_peers()
                .expect("Unable to get peer list");

            assert_eq!(
                peer_list,
                vec![PeerAuthorizationToken::from_peer_id("test_peer")]
            );
        }
        // drop peer_ref

        let peer_list = peer_connector
            .list_peers()
            .expect("Unable to get peer list");

        assert_eq!(peer_list, Vec::<PeerAuthorizationToken>::new());

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that when a EndpointPeerRef is dropped, a remove peer request is properly sent and the
    // peer is removed
    //
    // 1. add unidentified peer with endpoint inproc://test
    // 2. add test_peer
    // 4. call list peers
    // 5. verify that the peer list contains test_peer
    // 6. drop the PeerRef
    // 7. call list peers
    // 8. verify that the peer list still contains test_peer
    // 9. drop endpoint peer_ref
    // 10. call list peers
    // 11. verify that the new peer list is empty
    #[test]
    fn test_peer_manager_drop_endpoint_peer_ref() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        // let (finish_tx, fininsh_rx) = channel();
        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();

        {
            let (tx, notification_rx): (
                Sender<PeerManagerNotification>,
                mpsc::Receiver<PeerManagerNotification>,
            ) = channel();
            peer_connector
                .subscribe_sender(tx)
                .expect("Unable to get subscriber");
            let endpoint_peer_ref = peer_connector
                .add_unidentified_peer(
                    "inproc://test".to_string(),
                    PeerAuthorizationToken::from_peer_id("my_id"),
                )
                .expect("Unable to add peer by endpoint");
            assert_eq!(endpoint_peer_ref.endpoint(), "inproc://test".to_string());
            // timeout after 60 seconds
            let timeout = Duration::from_secs(60);
            let notification = notification_rx
                .recv_timeout(timeout)
                .expect("Unable to get new notifications");
            assert!(
                notification
                    == PeerManagerNotification::Connected {
                        peer: PeerTokenPair::new(
                            PeerAuthorizationToken::from_peer_id("test_peer"),
                            PeerAuthorizationToken::from_peer_id("my_id"),
                        )
                    }
            );

            let peer_ref = peer_connector
                .add_peer_ref(
                    PeerAuthorizationToken::from_peer_id("test_peer"),
                    vec!["inproc://test".to_string()],
                    PeerAuthorizationToken::from_peer_id("my_id"),
                )
                .expect("Unable to add peer");

            assert_eq!(
                peer_ref.peer_id(),
                &PeerTokenPair::new(
                    PeerAuthorizationToken::from_peer_id("test_peer"),
                    PeerAuthorizationToken::from_peer_id("my_id"),
                )
            );

            let peer_list = peer_connector
                .list_peers()
                .expect("Unable to get peer list");

            assert_eq!(
                peer_list,
                vec![PeerAuthorizationToken::from_peer_id("test_peer")]
            );

            drop(peer_ref);

            let peer_list = peer_connector
                .list_peers()
                .expect("Unable to get peer list");

            assert_eq!(
                peer_list,
                vec![PeerAuthorizationToken::from_peer_id("test_peer")]
            );
        }
        // drop endpoint_peer_ref

        let peer_list = peer_connector
            .list_peers()
            .expect("Unable to get peer list");

        assert_eq!(peer_list, Vec::<PeerAuthorizationToken>::new());

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that if a peer's endpoint disconnects and does not reconnect during a set timeout, the
    // PeerManager will retry the peers list of endpoints trying to find an endpoint that is
    // available.
    //
    // 1. add test_peer, this will connected to the first endpoint
    // 2. verify that the test_peer connection receives a heartbeat
    // 3. disconnect the connection made to test_peer
    // 4. verify that subscribers will receive a Disconnected notification
    // 5. drop the listener for the first endpoint to force the attempt on the second endpoint
    // 6. verify that subscribers will receive a Connected notification when the new active endpoint
    //    is successfully connected to.
    #[test]
    fn test_peer_manager_update_active_endpoint() {
        let mut transport = Box::new(RawTransport::default());
        let mut listener = transport
            .listen("tcp://localhost:0")
            .expect("Cannot listen for connections");
        let endpoint = listener.endpoint();
        let mut mesh1 = Mesh::new(512, 128);
        let mut mesh2 = Mesh::new(512, 128);

        let mut listener2 = transport
            .listen("tcp://localhost:0")
            .expect("Cannot listen for connections");
        let endpoint2 = listener2.endpoint();

        let (tx, rx) = mpsc::channel();
        let jh = thread::spawn(move || {
            // accept incoming connection and add it to mesh2
            let conn = listener.accept().expect("Cannot accept connection");
            mesh2
                .add(conn, "test_id".to_string())
                .expect("Cannot add connection to mesh");
            // Verify mesh received heartbeat
            let envelope = mesh2.recv().expect("Cannot receive message");
            let heartbeat: NetworkMessage = Message::parse_from_bytes(&envelope.payload())
                .expect("Cannot parse NetworkMessage");
            assert_eq!(
                heartbeat.get_message_type(),
                NetworkMessageType::NETWORK_HEARTBEAT
            );
            // remove connection to cause reconnection attempt
            let mut connection = mesh2
                .remove("test_id")
                .expect("Cannot remove connection from mesh");
            connection
                .disconnect()
                .expect("Connection failed to disconnect");
            // force drop of first listener
            drop(listener);
            // wait for the peer manager to switch endpoints
            let conn = listener2.accept().expect("Unable to accept connection");
            mesh2
                .add(conn, "test_id".to_string())
                .expect("Cannot add connection to mesh");

            rx.recv().unwrap();

            mesh2.signal_shutdown();
            mesh2.wait_for_shutdown().expect("Unable to shutdown mesh");
        });

        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new_multiple(&[
                "test_peer",
                "test_peer",
            ])))
            .with_matrix_life_cycle(mesh1.get_life_cycle())
            .with_matrix_sender(mesh1.get_sender())
            .with_transport(transport)
            .with_heartbeat_interval(1)
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_max_retry_attempts(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();
        let (notification_tx, notification_rx): (
            Sender<PeerManagerNotification>,
            mpsc::Receiver<PeerManagerNotification>,
        ) = channel();
        peer_connector
            .subscribe_sender(notification_tx)
            .expect("Unable to get subscriber");
        let peer_ref = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec![endpoint, endpoint2],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        // timeout after 60 seconds
        let timeout = Duration::from_secs(60);
        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        // receive reconnecting attempt
        let disconnected_notification = notification_rx
            .recv_timeout(timeout)
            .expect("Cannot get message from subscriber");
        assert!(
            disconnected_notification
                == PeerManagerNotification::Disconnected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        // receive notifications that the peer is connected to new endpoint
        let connected_notification = notification_rx
            .recv_timeout(timeout)
            .expect("Cannot get message from subscriber");

        assert!(
            connected_notification
                == PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                }
        );

        tx.send(()).unwrap();

        jh.join().unwrap();
        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh1.signal_shutdown();
        mesh1.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that the PeerManager can be started and stopped
    #[test]
    fn test_peer_manager_shutdown() {
        let transport = Box::new(InprocTransport::default());

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that the PeerManager can receive incoming peer requests and handle them appropriately.
    //
    // 1. Add a connection
    // 2. Verify that it has been added as a unreferenced peer
    // 3. Verify that it can be promoted to a proper peer
    #[test]
    fn test_incoming_peer_request() {
        let mut transport = InprocTransport::default();
        let mut listener = transport.listen("inproc://test").unwrap();

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(Box::new(transport.clone()))
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();

        let recv_connector = connector.clone();
        let jh = thread::spawn(move || {
            let connection = listener.accept().unwrap();
            let (subs_tx, subs_rx): (mpsc::Sender<ConnectionManagerNotification>, _) =
                mpsc::channel();
            let _ = recv_connector
                .subscribe(subs_tx)
                .expect("unable to get subscriber");
            recv_connector.add_inbound_connection(connection).unwrap();
            // wait for inbound connection notification to come
            subs_rx.recv().expect("unable to get notification");
        });

        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();

        let _conn = transport.connect("inproc://test").unwrap();

        jh.join().unwrap();

        // The peer is not part of the set of active peers
        assert!(peer_connector.list_peers().unwrap().is_empty());

        assert_eq!(
            vec![PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )],
            peer_connector.list_unreferenced_peers().unwrap()
        );

        let peer_ref = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        let peer_list = peer_connector
            .list_peers()
            .expect("Unable to get peer list");

        assert_eq!(
            peer_list,
            vec![PeerAuthorizationToken::from_peer_id("test_peer")]
        );

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that the PeerManager can be started with the deprecated PeerManager::new() and
    // PeerManger.start() function. This tests intentionally uses deprecated methods so the
    // deprecated warnings are ignored.
    #[test]
    fn test_peer_manager_no_builder() {
        let transport = Box::new(InprocTransport::default());

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();

        #[allow(deprecated)]
        let mut peer_manager =
            PeerManager::new(connector, Some(1), Some(1), "my_id".to_string(), true);

        #[allow(deprecated)]
        peer_manager.start().expect("Cannot start peer_manager");

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    // Test that a subscriber can convert the PeerManagerNotification to another type
    //
    // 1. add test_peer
    // 2. verify that the returned PeerRef contains the test_peer id
    // 3. verify the the a Connected notification is received and is converted to a TestEnum
    #[test]
    fn test_peer_manager_notifciation_convert() {
        let mut transport = Box::new(InprocTransport::default());
        let mut listener = transport.listen("inproc://test").unwrap();

        thread::spawn(move || {
            listener.accept().unwrap();
        });

        let mut mesh = Mesh::new(512, 128);
        let mut cm = ConnectionManager::builder()
            .with_authorizer(Box::new(NoopAuthorizer::new("test_peer")))
            .with_matrix_life_cycle(mesh.get_life_cycle())
            .with_matrix_sender(mesh.get_sender())
            .with_transport(transport.clone())
            .start()
            .expect("Unable to start Connection Manager");

        let connector = cm.connector();
        let mut peer_manager = PeerManager::builder()
            .with_connector(connector)
            .with_retry_interval(1)
            .with_identity("my_id".to_string())
            .with_strict_ref_counts(true)
            .start()
            .expect("Cannot start peer_manager");
        let peer_connector = peer_manager.connector();
        let (tx, notification_rx): (Sender<TestEnum>, mpsc::Receiver<TestEnum>) = channel();
        peer_connector
            .subscribe_sender(tx)
            .expect("Unable to get subscriber");
        let peer_ref = peer_connector
            .add_peer_ref(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                vec!["inproc://test".to_string()],
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
            .expect("Unable to add peer");

        assert_eq!(
            peer_ref.peer_id(),
            &PeerTokenPair::new(
                PeerAuthorizationToken::from_peer_id("test_peer"),
                PeerAuthorizationToken::from_peer_id("my_id"),
            )
        );

        // timeout after 60 seconds
        let timeout = Duration::from_secs(60);
        let notification = notification_rx
            .recv_timeout(timeout)
            .expect("Unable to get new notifications");
        assert!(
            notification
                == TestEnum::Notification(PeerManagerNotification::Connected {
                    peer: PeerTokenPair::new(
                        PeerAuthorizationToken::from_peer_id("test_peer"),
                        PeerAuthorizationToken::from_peer_id("my_id"),
                    )
                })
        );

        peer_manager.signal_shutdown();
        cm.signal_shutdown();
        peer_manager
            .wait_for_shutdown()
            .expect("Unable to shutdown peer manager");
        cm.wait_for_shutdown()
            .expect("Unable to shutdown connection manager");
        mesh.signal_shutdown();
        mesh.wait_for_shutdown().expect("Unable to shutdown mesh");
    }

    #[derive(PartialEq)]
    enum TestEnum {
        Notification(PeerManagerNotification),
    }
    /// Converts `PeerManagerNotification` into `Test_Enum::Notification(PeerManagerNotification)`
    impl From<PeerManagerNotification> for TestEnum {
        fn from(notification: PeerManagerNotification) -> Self {
            TestEnum::Notification(notification)
        }
    }

    struct NoopAuthorizer {
        ids: std::cell::RefCell<VecDeque<String>>,
    }

    impl NoopAuthorizer {
        fn new(id: &str) -> Self {
            let mut ids = VecDeque::new();
            ids.push_back(id.into());
            Self {
                ids: std::cell::RefCell::new(ids),
            }
        }

        fn new_multiple(ids: &[&str]) -> Self {
            Self {
                ids: std::cell::RefCell::new(
                    ids.iter().map(std::string::ToString::to_string).collect(),
                ),
            }
        }
    }

    impl Authorizer for NoopAuthorizer {
        fn authorize_connection(
            &self,
            connection_id: String,
            connection: Box<dyn Connection>,
            callback: Box<
                dyn Fn(AuthorizationResult) -> Result<(), Box<dyn std::error::Error>> + Send,
            >,
            _expected_authorization: Option<ConnectionAuthorizationType>,
            local_authorization: Option<ConnectionAuthorizationType>,
        ) -> Result<(), AuthorizerError> {
            let identity = self
                .ids
                .borrow_mut()
                .pop_front()
                .expect("No more identities to provide");
            (*callback)(AuthorizationResult::Authorized {
                connection_id,
                connection,
                identity: ConnectionAuthorizationType::Trust {
                    identity: identity.clone(),
                },
                expected_authorization: ConnectionAuthorizationType::Trust { identity },
                local_authorization: local_authorization.unwrap_or(
                    ConnectionAuthorizationType::Trust {
                        identity: "my_id".into(),
                    },
                ),
            })
            .map_err(|err| AuthorizerError(format!("Unable to return result: {}", err)))
        }
    }
}
