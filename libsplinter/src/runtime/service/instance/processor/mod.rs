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

mod builder;
mod error;
pub(crate) mod registry;
mod sender;

use crossbeam_channel::{Receiver, Sender};
use protobuf::Message;
use uuid::Uuid;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::channel;
use crate::mesh::{Envelope, Mesh, RecvTimeoutError as MeshRecvTimeoutError};
use crate::network::reply::InboundRouter;
use crate::protocol::network::NetworkMessage;
use crate::protos::circuit::{
    AdminDirectMessage, CircuitDirectMessage, CircuitError, CircuitMessage, CircuitMessageType,
    ServiceConnectResponse, ServiceDisconnectResponse,
};
use crate::protos::prelude::*;
use crate::service::instance::{ServiceInstance, ServiceMessageContext};
use crate::threading::lifecycle::ShutdownHandle;
use crate::transport::Connection;
use crate::{rwlock_read_unwrap, rwlock_write_unwrap};

use self::registry::StandardServiceNetworkRegistry;
use self::sender::{ProcessorMessage, ServiceMessage};

pub use builder::ServiceProcessorBuilder;
pub use error::ServiceProcessorError;

// Recv timeout in secs
const TIMEOUT_SEC: u64 = 2;

/// State that can be passed between threads.
/// Includes the service senders and join_handles for the service threads.
struct SharedState {
    pub services: HashMap<String, Sender<ProcessorMessage>>,
}

/// Helper macro for generating ServiceProcessorError::ProcessError
macro_rules! process_err {
    ($err:ident, $ctx_msg:expr) => {
        ServiceProcessorError::ProcessError($ctx_msg.into(), Box::new($err))
    };
    ($err:ident, $ctx_msg:tt, $($fmt_arg:tt)*) => {
        ServiceProcessorError::ProcessError(format!($ctx_msg, $($fmt_arg)*), Box::new($err))
    }
}

/// Helper macro for generating map_err functions that convert errors into
/// ServiceProcessorError::ProcessError values.
macro_rules! to_process_err {
    ($($arg:tt)*) => {
        |err| process_err!(err, $($arg)*)
    }
}

type ShutdownSignalFn = Box<dyn Fn() -> Result<(), ServiceProcessorError> + Send>;

/// The ServiceProcessor handles the networking for services. This includes talking to the
/// splinter node, connecting for authorization, registering the services, and routing
/// direct messages to the correct service.
pub struct ServiceProcessor {
    shared_state: Arc<RwLock<SharedState>>,
    services: Vec<Box<dyn ServiceInstance>>,
    mesh: Mesh,
    circuit: String,
    node_mesh_id: String,
    network_sender: Sender<Vec<u8>>,
    network_receiver: Receiver<Vec<u8>>,
    inbound_router: InboundRouter<CircuitMessageType>,
    inbound_receiver: Receiver<Result<(CircuitMessageType, Vec<u8>), channel::RecvError>>,
    channel_capacity: usize,
}

impl ServiceProcessor {
    pub fn new(
        connection: Box<dyn Connection>,
        circuit: String,
        incoming_capacity: usize,
        outgoing_capacity: usize,
        channel_capacity: usize,
    ) -> Result<Self, ServiceProcessorError> {
        let mesh = Mesh::new(incoming_capacity, outgoing_capacity);
        let node_mesh_id = format!("{}", Uuid::new_v4());
        mesh.add(connection, node_mesh_id.to_string())
            .map_err(|err| process_err!(err, "unable to add connection to mesh"))?;
        let (network_sender, network_receiver) = crossbeam_channel::bounded(channel_capacity);
        let (inbound_sender, inbound_receiver) = crossbeam_channel::bounded(channel_capacity);
        Ok(ServiceProcessor {
            shared_state: Arc::new(RwLock::new(SharedState {
                services: HashMap::new(),
            })),
            services: vec![],
            mesh,
            circuit,
            node_mesh_id,
            network_sender,
            network_receiver,
            inbound_router: InboundRouter::new(Box::new(inbound_sender)),
            inbound_receiver,
            channel_capacity,
        })
    }

    /// add_service takes a Service and sets up the thread that the service will run in.
    /// The service will be started, including registration and then messages are routed to the
    /// the services using a channel.
    pub fn add_service(
        &mut self,
        service: Box<dyn ServiceInstance>,
    ) -> Result<(), ServiceProcessorError> {
        if self
            .services
            .iter()
            .any(|s| s.service_id() == service.service_id())
        {
            Err(ServiceProcessorError::AddServiceError(format!(
                "{} already exists",
                service.service_id()
            )))
        } else {
            self.services.push(service);

            Ok(())
        }
    }

    /// Once the service processor is started it will handle incoming messages from the splinter
    /// node and route it to a running service.
    ///
    /// Returns a [ShutdownHandle] impelmentation so the service can be properly shutdown.
    pub fn start(self) -> Result<ServiceProcessorShutdownHandle, ServiceProcessorError> {
        self.do_start().map(
            |(do_shutdown, join_handles)| ServiceProcessorShutdownHandle {
                signal_shutdown: do_shutdown,
                join_handles: Some(join_handles),
            },
        )
    }

    fn do_start(
        self,
    ) -> Result<
        (
            ShutdownSignalFn,
            JoinHandles<Result<(), ServiceProcessorError>>,
        ),
        ServiceProcessorError,
    > {
        let running = Arc::new(AtomicBool::new(true));
        let mut join_handles = vec![];
        for service in self.services.into_iter() {
            let mut shared_state = rwlock_write_unwrap!(self.shared_state);
            let service_id = service.service_id().to_string();

            let (send, recv) = crossbeam_channel::bounded(self.channel_capacity);
            let network_sender = self.network_sender.clone();
            let circuit = self.circuit.clone();
            let inbound_router = self.inbound_router.clone();
            let join_handle = thread::Builder::new()
                .name(format!("Service {}", service_id))
                .spawn(move || {
                    let service_id = service.service_id().to_string();
                    if let Err(err) =
                        run_service_loop(circuit, service, network_sender, recv, inbound_router)
                    {
                        error!("Terminating service {} due to error: {}", service_id, err);
                        Err(err)
                    } else {
                        Ok(())
                    }
                })?;
            join_handles.push(join_handle);
            shared_state.services.insert(service_id.to_string(), send);
        }

        let incoming_mesh = self.mesh.clone();
        let shared_state = self.shared_state.clone();
        let incoming_running = running.clone();
        let mut inbound_router = self.inbound_router.clone();
        // Thread to handle incoming messages from a splinter node.
        let incoming_join_handle: JoinHandle<Result<(), ServiceProcessorError>> =
            thread::Builder::new()
                .name("ServiceProcessor incoming".into())
                .spawn(move || {
                    while incoming_running.load(Ordering::SeqCst) {
                        let timeout = Duration::from_secs(TIMEOUT_SEC);
                        let message_bytes = match incoming_mesh.recv_timeout(timeout) {
                            Ok(envelope) => Vec::from(envelope),
                            Err(MeshRecvTimeoutError::Timeout) => continue,
                            Err(MeshRecvTimeoutError::Disconnected) => {
                                error!("Mesh Disconnected");
                                break;
                            }
                            Err(MeshRecvTimeoutError::PoisonedLock) => {
                                error!("Mesh lock was poisoned");
                                break;
                            }
                            Err(MeshRecvTimeoutError::Shutdown) => {
                                error!("Mesh has shutdown");
                                break;
                            }
                        };

                        if let Err(err) = process_incoming_msg(&message_bytes, &mut inbound_router)
                        {
                            error!("Unable to process message: {}", err);
                            continue;
                        }
                    }

                    Ok(())
                })?;

        let inbound_receiver = self.inbound_receiver;
        let inbound_running = running.clone();
        // Thread that handles messages that do not have a matching correlation id
        let inbound_join_handle: JoinHandle<Result<(), ServiceProcessorError>> =
            thread::Builder::new()
                .name("Handle message with correlation_id".into())
                .spawn(move || {
                    let timeout = Duration::from_secs(TIMEOUT_SEC);
                    while inbound_running.load(Ordering::SeqCst) {
                        let service_message = match inbound_receiver.recv_timeout(timeout) {
                            Ok(msg) => msg,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                            Err(err) => {
                                debug!("inbound sender dropped; ending inbound message thread");
                                return Err(process_err!(err, "inbound sender dropped"));
                            }
                        }
                        .map_err(to_process_err!("received service message error"))?;

                        if let Err(err) =
                            process_inbound_msg_with_correlation_id(service_message, &shared_state)
                        {
                            error!("Unable to process inbound message: {}", err);
                        }
                    }

                    Ok(())
                })?;

        let outgoing_mesh = self.mesh;
        let outgoing_running = running.clone();
        let outgoing_receiver = self.network_receiver;
        let node_mesh_id = self.node_mesh_id.to_string();

        // Thread that handles outgoing messages that need to be sent to the splinter node
        let outgoing_join_handle: JoinHandle<Result<(), ServiceProcessorError>> =
            thread::Builder::new()
                .name("ServiceProcessor outgoing".into())
                .spawn(move || {
                    while outgoing_running.load(Ordering::SeqCst) {
                        let timeout = Duration::from_secs(TIMEOUT_SEC);
                        let message_bytes = match outgoing_receiver.recv_timeout(timeout) {
                            Ok(msg) => msg,
                            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                            Err(err) => {
                                error!("channel dropped while handling outgoing messages: {}", err);
                                break;
                            }
                        };

                        // Send message to splinter node
                        if let Err(err) = outgoing_mesh
                            .send(Envelope::new(node_mesh_id.to_string(), message_bytes))
                        {
                            error!(
                                "Unable to send message via mesh to {}: {}",
                                node_mesh_id, err
                            );
                            continue;
                        }
                    }

                    Ok(())
                })?;

        let service_shutdown_join_handle: JoinHandle<Result<(), ServiceProcessorError>> =
            thread::Builder::new()
                .name("ServiceProcessorShutdownMonitor".into())
                .spawn(move || {
                    while let Some(join_handle) = join_handles.pop() {
                        match join_handle.join() {
                            Ok(Ok(_)) => (),
                            Ok(Err(err)) => {
                                error!("A service thread exited with an error: {}", err);
                            }
                            Err(err) => {
                                error!("A service thread had panicked: {:?}", err);
                            }
                        }
                    }
                    running.store(false, Ordering::SeqCst);
                    Ok(())
                })?;

        let shutdown_shared_state = self.shared_state;
        // Creates the shutdown handle that will be called by the process starting up the
        // Service processor
        let do_shutdown = Box::new(move || {
            debug!("Shutting down service processor");
            let shared_state = rwlock_write_unwrap!(shutdown_shared_state);
            // send shutdown to the services and wait for join
            for (service_id, service_sender) in shared_state.services.iter() {
                info!("Shutting down {}", service_id);
                service_sender
                    .send(ProcessorMessage::Shutdown)
                    .map_err(|err| {
                        ServiceProcessorError::ShutdownError(format!(
                            "unable to send shutdown message: {:?}",
                            err
                        ))
                    })?;
            }

            Ok(())
        });

        Ok((
            do_shutdown,
            JoinHandles::new(vec![
                // order matters here -> when this thread completes, it will have signaled the
                // remaining threads to also shutdown.
                service_shutdown_join_handle,
                incoming_join_handle,
                outgoing_join_handle,
                inbound_join_handle,
            ]),
        ))
    }
}

fn process_incoming_msg(
    message_bytes: &[u8],
    inbound_router: &mut InboundRouter<CircuitMessageType>,
) -> Result<(), ServiceProcessorError> {
    let msg = NetworkMessage::from_bytes(message_bytes)
        .map_err(to_process_err!("unable parse network message"))?;

    // if a service is waiting on a reply the inbound router will
    // route back the reponse to the service based on the correlation id in
    // the message, otherwise it will be sent to the inbound thread
    match msg {
        NetworkMessage::Circuit(payload) => {
            let mut circuit_msg: CircuitMessage = Message::parse_from_bytes(&payload)
                .map_err(to_process_err!("unable to parse circuit message"))?;

            match circuit_msg.get_message_type() {
                CircuitMessageType::ADMIN_DIRECT_MESSAGE => {
                    let admin_direct_message: AdminDirectMessage =
                        Message::parse_from_bytes(circuit_msg.get_payload())
                            .map_err(to_process_err!("unable to parse admin direct message"))?;
                    inbound_router
                        .route(
                            admin_direct_message.get_correlation_id(),
                            Ok((
                                CircuitMessageType::ADMIN_DIRECT_MESSAGE,
                                circuit_msg.take_payload(),
                            )),
                        )
                        .map_err(to_process_err!("unable to route message"))?;
                }
                CircuitMessageType::CIRCUIT_DIRECT_MESSAGE => {
                    let direct_message: CircuitDirectMessage =
                        Message::parse_from_bytes(circuit_msg.get_payload())
                            .map_err(to_process_err!("unable to parse circuit direct message"))?;
                    inbound_router
                        .route(
                            direct_message.get_correlation_id(),
                            Ok((
                                CircuitMessageType::CIRCUIT_DIRECT_MESSAGE,
                                circuit_msg.take_payload(),
                            )),
                        )
                        .map_err(to_process_err!("unable to route message"))?;
                }
                CircuitMessageType::SERVICE_CONNECT_RESPONSE => {
                    let response: ServiceConnectResponse =
                        Message::parse_from_bytes(circuit_msg.get_payload())
                            .map_err(to_process_err!("unable to parse service connect response"))?;
                    inbound_router
                        .route(
                            response.get_correlation_id(),
                            Ok((
                                CircuitMessageType::SERVICE_CONNECT_RESPONSE,
                                circuit_msg.take_payload(),
                            )),
                        )
                        .map_err(to_process_err!("unable to route message"))?;
                }
                CircuitMessageType::SERVICE_DISCONNECT_RESPONSE => {
                    let response: ServiceDisconnectResponse =
                        Message::parse_from_bytes(circuit_msg.get_payload()).map_err(|err| {
                            process_err!(err, "unable to parse service disconnect response")
                        })?;
                    inbound_router
                        .route(
                            response.get_correlation_id(),
                            Ok((
                                CircuitMessageType::SERVICE_DISCONNECT_RESPONSE,
                                circuit_msg.take_payload(),
                            )),
                        )
                        .map_err(to_process_err!("unable to route message"))?;
                }
                msg_type => warn!("Received unimplemented message: {:?}", msg_type),
            }
        }
        NetworkMessage::NetworkHeartbeat(_) => trace!("Received network heartbeat"),
        _ => warn!("Received unimplemented message"),
    }

    Ok(())
}

fn process_inbound_msg_with_correlation_id(
    service_message: (CircuitMessageType, Vec<u8>),
    shared_state: &Arc<RwLock<SharedState>>,
) -> Result<(), ServiceProcessorError> {
    match service_message {
        (CircuitMessageType::ADMIN_DIRECT_MESSAGE, msg) => {
            let admin_direct_message: AdminDirectMessage = Message::parse_from_bytes(&msg)
                .map_err(to_process_err!(
                    "unable to parse inbound admin direct message"
                ))?;

            handle_admin_direct_msg(admin_direct_message, shared_state).map_err(
                to_process_err!("unable to handle inbound admin direct message"),
            )?;
        }
        (CircuitMessageType::CIRCUIT_DIRECT_MESSAGE, msg) => {
            let circuit_direct_message: CircuitDirectMessage = Message::parse_from_bytes(&msg)
                .map_err(to_process_err!(
                    "unable to parse inbound circuit direct message"
                ))?;

            handle_circuit_direct_msg(circuit_direct_message, shared_state).map_err(
                to_process_err!("unable to handle inbound circuit direct message"),
            )?;
        }
        (CircuitMessageType::CIRCUIT_ERROR_MESSAGE, msg) => {
            let response: CircuitError = Message::parse_from_bytes(&msg)
                .map_err(to_process_err!("unable to parse circuit error message"))?;
            warn!("Received circuit error message {:?}", response);
        }
        (msg_type, _) => warn!(
            "Received message ({:?}) that does not have a correlation id",
            msg_type
        ),
    }
    Ok(())
}

pub struct JoinHandles<T> {
    join_handles: Vec<JoinHandle<T>>,
}

impl<T> JoinHandles<T> {
    fn new(join_handles: Vec<JoinHandle<T>>) -> Self {
        Self { join_handles }
    }

    pub fn join_all(self) -> thread::Result<Vec<T>> {
        let mut res = Vec::with_capacity(self.join_handles.len());

        for jh in self.join_handles.into_iter() {
            res.push(jh.join()?);
        }

        Ok(res)
    }
}

pub struct ServiceProcessorShutdownHandle {
    signal_shutdown: Box<dyn Fn() -> Result<(), ServiceProcessorError> + Send>,
    join_handles: Option<JoinHandles<Result<(), ServiceProcessorError>>>,
}

impl ShutdownHandle for ServiceProcessorShutdownHandle {
    fn signal_shutdown(&mut self) {
        if let Err(err) = (*self.signal_shutdown)() {
            error!("Unable to signal service processor to shutdown: {}", err);
        }
    }

    fn wait_for_shutdown(mut self) -> Result<(), crate::error::InternalError> {
        if let Some(join_handles) = self.join_handles.take() {
            match join_handles.join_all() {
                Ok(results) => {
                    results
                        .into_iter()
                        .filter(Result::is_err)
                        .map(Result::unwrap_err)
                        .for_each(|err| {
                            error!("{}", err);
                        });
                }
                Err(_) => {
                    return Err(crate::error::InternalError::with_message(
                        "Unable to join service processor threads".into(),
                    ));
                }
            }
        }

        Ok(())
    }
}

fn run_service_loop(
    circuit: String,
    mut service: Box<dyn ServiceInstance>,
    network_sender: Sender<Vec<u8>>,
    service_recv: Receiver<ProcessorMessage>,
    inbound_router: InboundRouter<CircuitMessageType>,
) -> Result<(), ServiceProcessorError> {
    info!("Starting Service: {}", service.service_id());
    let registry = StandardServiceNetworkRegistry::new(circuit, network_sender, inbound_router);
    service.start(&registry).map_err(to_process_err!(
        "unable to start service {}",
        service.service_id()
    ))?;

    loop {
        let service_message: ServiceMessage = match service_recv.recv() {
            Ok(ProcessorMessage::ServiceMessage(message)) => Ok(message),
            Ok(ProcessorMessage::Shutdown) => {
                info!("Shutting down {}", service.service_id());
                service
                    .stop(&registry)
                    .map_err(to_process_err!("unable to stop service"))?;
                service
                    .destroy()
                    .map_err(to_process_err!("unable to destroy service"))?;
                break;
            }
            Err(err) => Err(process_err!(err, "unable to receive service messages")),
        }?;

        match service_message {
            ServiceMessage::AdminDirectMessage(mut admin_direct_message) => {
                let msg_context = ServiceMessageContext {
                    sender: admin_direct_message.take_sender(),
                    circuit: admin_direct_message.take_circuit(),
                    correlation_id: admin_direct_message.take_correlation_id(),
                };

                if let Err(err) =
                    service.handle_message(admin_direct_message.get_payload(), &msg_context)
                {
                    error!("unable to handle admin direct message: {}", err);
                }
            }
            ServiceMessage::CircuitDirectMessage(mut direct_message) => {
                let msg_context = ServiceMessageContext {
                    sender: direct_message.take_sender(),
                    circuit: direct_message.take_circuit(),
                    correlation_id: direct_message.take_correlation_id(),
                };

                if let Err(err) = service.handle_message(direct_message.get_payload(), &msg_context)
                {
                    error!("unable to handle circuit direct message: {}", err);
                }
            }
        }
    }
    Ok(())
}

fn handle_circuit_direct_msg(
    direct_message: CircuitDirectMessage,
    shared_state: &Arc<RwLock<SharedState>>,
) -> Result<(), ServiceProcessorError> {
    let shared_state = rwlock_read_unwrap!(shared_state);

    if let Some(service_sender) = shared_state.services.get(direct_message.get_recipient()) {
        service_sender
            .send(ProcessorMessage::ServiceMessage(
                ServiceMessage::CircuitDirectMessage(direct_message),
            ))
            .map_err(to_process_err!(
                "unable to send service (circuit direct) message"
            ))?;
    } else {
        warn!(
            "Service with id {} does not exist, ignoring message",
            direct_message.get_recipient()
        );
    }
    Ok(())
}

fn handle_admin_direct_msg(
    admin_direct_message: AdminDirectMessage,
    shared_state: &Arc<RwLock<SharedState>>,
) -> Result<(), ServiceProcessorError> {
    let shared_state = rwlock_read_unwrap!(shared_state);

    if let Some(service_sender) = shared_state
        .services
        .get(admin_direct_message.get_recipient())
    {
        service_sender
            .send(ProcessorMessage::ServiceMessage(
                ServiceMessage::AdminDirectMessage(admin_direct_message),
            ))
            .map_err(to_process_err!(
                "unable to send service (admin direct) message"
            ))?;
    } else {
        warn!(
            "Service with id {} does not exist, ignoring message",
            admin_direct_message.get_recipient()
        );
    }
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    use std::any::Any;
    use std::sync::mpsc::channel;
    use std::thread;

    use protobuf::Message;

    use crate::mesh::Mesh;
    use crate::protos::circuit::{
        ServiceConnectRequest, ServiceConnectResponse_Status, ServiceDisconnectRequest,
        ServiceDisconnectResponse_Status,
    };
    use crate::protos::network::NetworkMessage;
    use crate::service::instance::{
        ServiceDestroyError, ServiceError, ServiceNetworkRegistry, ServiceNetworkSender,
        ServiceStartError, ServiceStopError,
    };
    use crate::transport::inproc::InprocTransport;
    use crate::transport::matrix::ConnectionMatrixSender;
    use crate::transport::Transport;

    use super::sender::create_message;

    #[test]
    // This test uses a MockService that will call the corresponding network_sender function.
    // Verifies that the ServiceProcessor sends a connect request, starts up the service, and
    // route the messages to the service, including routing through the inbound router when there
    // is a matching correlation id.
    fn standard_direct_message() {
        let mut transport = InprocTransport::default();
        let mut inproc_listener = transport.listen("internal").unwrap();

        let mesh = Mesh::new(512, 128);
        let mesh_sender = mesh.get_sender();

        let (tx, rx) = channel();

        let jh = thread::Builder::new()
            .name("standard_direct_message".to_string())
            .spawn(move || {
                let connection = transport.connect("internal").unwrap();
                let mut processor =
                    ServiceProcessor::new(connection, "alpha".to_string(), 3, 3, 3).unwrap();

                // Add MockService to the processor and start the processor.
                let service = MockService::new();
                processor.add_service(Box::new(service)).unwrap();

                let mut shutdown_handle = processor.start().unwrap();
                let _ = rx.recv().unwrap();

                shutdown_handle.signal_shutdown();
                let _ = rx.recv().unwrap();

                shutdown_handle
                    .wait_for_shutdown()
                    .expect("Unable to cleanly shutdown");
            })
            .unwrap();

        // this part of the test mimics the splinter daemon sending message to the connected
        // service
        let connection = inproc_listener.accept().unwrap();
        mesh.add(connection, "service_processor".to_string())
            .unwrap();

        // Receive service connect request and respond with ServiceConnectionResposne with status
        // OK
        let mut service_request = get_service_connect(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(service_request.get_service_id(), "mock_service");
        assert_eq!(service_request.get_circuit(), "alpha");

        let service_response = create_service_connect_response(
            service_request.take_correlation_id(),
            "alpha".to_string(),
        )
        .unwrap();
        mesh_sender
            .send("service_processor".to_string(), service_response)
            .unwrap();

        // request the mock service sends a message without caring about correlation id
        let send_msg = create_circuit_direct_msg(b"send".to_vec()).unwrap();
        mesh_sender
            .send("service_processor".to_string(), send_msg)
            .unwrap();

        let send_response = get_circuit_direct_msg(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(send_response.get_payload(), b"send_response");

        // request the mock service send_and_await a message and blocks until correlation id is
        // returned
        let send_and_await_msg = create_circuit_direct_msg(b"send_and_await".to_vec()).unwrap();
        mesh_sender
            .send("service_processor".to_string(), send_and_await_msg)
            .unwrap();

        let mut waiting_response = get_circuit_direct_msg(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(waiting_response.get_payload(), b"waiting for response");

        // respond to send_and_await
        let wait_response = create_circuit_direct_msg_with_correlation_id(
            b"respond to waiting".to_vec(),
            waiting_response.take_correlation_id(),
        )
        .unwrap();
        mesh_sender
            .send("service_processor".to_string(), wait_response)
            .unwrap();

        // reply to this provided message
        let reply_request = create_circuit_direct_msg_with_correlation_id(
            b"reply".to_vec(),
            "reply_correlation_id".to_string(),
        )
        .unwrap();
        mesh_sender
            .send("service_processor".to_string(), reply_request)
            .unwrap();

        let reply_response = get_circuit_direct_msg(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(reply_response.get_payload(), b"reply response");
        assert_eq!(reply_response.get_correlation_id(), "reply_correlation_id");

        tx.send("signal-shutdown").unwrap();

        let mut disconnect_req = get_service_disconnect(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(disconnect_req.get_service_id(), "mock_service");
        assert_eq!(disconnect_req.get_circuit(), "alpha");

        mesh_sender
            .send(
                "service_processor".to_string(),
                create_service_disconnect_response(
                    disconnect_req.take_correlation_id(),
                    "alpha".to_string(),
                )
                .unwrap(),
            )
            .unwrap();

        tx.send("wait-for-shutdown").unwrap();
        jh.join().unwrap();
    }

    #[test]
    fn test_admin_direct_message() {
        let mut transport = InprocTransport::default();
        let mut inproc_listener = transport.listen("internal").unwrap();

        let mesh = Mesh::new(512, 128);
        let mesh_sender = mesh.get_sender();

        let (tx, rx) = channel();
        let jh = thread::Builder::new()
            .name("test_admin_direct_message".to_string())
            .spawn(move || {
                let connection = transport.connect("internal").unwrap();
                let mut processor =
                    ServiceProcessor::new(connection, "admin".to_string(), 3, 3, 3).unwrap();

                // Add MockService to the processor and start the processor.
                let service = MockAdminService::new();
                processor.add_service(Box::new(service)).unwrap();

                let mut shutdown_handle = processor.start().unwrap();

                let _ = rx.recv().unwrap();
                shutdown_handle.signal_shutdown();

                let _ = rx.recv().unwrap();

                shutdown_handle
                    .wait_for_shutdown()
                    .expect("Unable to cleanly shutdown");
            })
            .unwrap();

        // this part of the test mimics the splinter daemon sending message to the connected
        // service
        let connection = inproc_listener.accept().unwrap();
        mesh.add(connection, "service_processor".to_string())
            .unwrap();

        // Receive service connect request and respond with ServiceConnectionResposne with status
        // OK
        let mut service_request = get_service_connect(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(service_request.get_service_id(), "mock_service");
        assert_eq!(service_request.get_circuit(), "admin");

        let service_response = create_service_connect_response(
            service_request.take_correlation_id(),
            "admin".to_string(),
        )
        .unwrap();
        mesh_sender
            .send("service_processor".to_string(), service_response)
            .unwrap();

        // request the mock service sends a message without caring about correlation id
        let send_msg = create_admin_direct_msg(b"send".to_vec()).unwrap();
        mesh_sender
            .send("service_processor".to_string(), send_msg)
            .unwrap();

        let send_response = get_admin_direct_msg(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(send_response.get_payload(), b"send_response");

        // request the mock service send_and_await a message and blocks until correlation id is
        // returned
        let send_and_await_msg = create_admin_direct_msg(b"send_and_await".to_vec()).unwrap();
        mesh_sender
            .send("service_processor".to_string(), send_and_await_msg)
            .unwrap();

        let mut waiting_response = get_admin_direct_msg(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(waiting_response.get_payload(), b"waiting for response");

        // respond to send_and_await
        let wait_response = create_admin_direct_msg_with_correlation_id(
            b"respond to waiting".to_vec(),
            waiting_response.take_correlation_id(),
        )
        .unwrap();
        mesh_sender
            .send("service_processor".to_string(), wait_response)
            .unwrap();

        // reply to this provided message
        let reply_request = create_admin_direct_msg_with_correlation_id(
            b"reply".to_vec(),
            "reply_correlation_id".to_string(),
        )
        .unwrap();
        mesh_sender
            .send("service_processor".to_string(), reply_request)
            .unwrap();

        let reply_response = get_admin_direct_msg(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(reply_response.get_payload(), b"reply response");
        assert_eq!(reply_response.get_correlation_id(), "reply_correlation_id");

        tx.send("signal-shutdown").unwrap();

        let mut disconnect_req = get_service_disconnect(mesh.recv().unwrap().payload().to_vec());
        assert_eq!(disconnect_req.get_service_id(), "mock_service");
        assert_eq!(disconnect_req.get_circuit(), "admin");

        mesh_sender
            .send(
                "service_processor".to_string(),
                create_service_disconnect_response(
                    disconnect_req.take_correlation_id(),
                    "admin".to_string(),
                )
                .unwrap(),
            )
            .unwrap();

        tx.send("wait-for-shutdown").unwrap();

        jh.join().unwrap();
    }

    // Service that can be used for testing a standard service's functionality
    struct MockService {
        service_id: String,
        service_type: String,
        network_sender: Option<Box<dyn ServiceNetworkSender>>,
    }

    impl MockService {
        pub fn new() -> Self {
            MockService {
                service_id: "mock_service".to_string(),
                service_type: "mock".to_string(),
                network_sender: None,
            }
        }
    }

    impl ServiceInstance for MockService {
        /// This service's id
        fn service_id(&self) -> &str {
            &self.service_id
        }

        /// This service's message family
        fn service_type(&self) -> &str {
            &self.service_type
        }

        /// Starts the service
        fn start(
            &mut self,
            service_registry: &dyn ServiceNetworkRegistry,
        ) -> Result<(), ServiceStartError> {
            let network_sender = service_registry.connect(self.service_id())?;
            self.network_sender = Some(network_sender);
            Ok(())
        }

        /// Stops the service
        fn stop(
            &mut self,
            service_registry: &dyn ServiceNetworkRegistry,
        ) -> Result<(), ServiceStopError> {
            service_registry.disconnect(self.service_id())?;
            Ok(())
        }

        /// Clean-up any resources before the service is removed.
        /// Consumes the service (which, given the use of dyn traits,
        /// this must take a boxed Service instance).
        fn destroy(self: Box<Self>) -> Result<(), ServiceDestroyError> {
            Ok(())
        }

        fn purge(&mut self) -> Result<(), crate::error::InternalError> {
            unimplemented!()
        }

        fn handle_message(
            &self,
            message_bytes: &[u8],
            message_context: &ServiceMessageContext,
        ) -> Result<(), ServiceError> {
            if message_bytes == b"send" {
                if let Some(network_sender) = &self.network_sender {
                    network_sender
                        .send(&message_context.sender, b"send_response")
                        .unwrap();
                }
            } else if message_bytes == b"send_and_await" {
                if let Some(network_sender) = &self.network_sender {
                    let response = network_sender
                        .send_and_await(&message_context.sender, b"waiting for response")
                        .unwrap();
                    assert_eq!(response, b"respond to waiting");
                }
            } else if message_bytes == b"reply" {
                if let Some(network_sender) = &self.network_sender {
                    network_sender
                        .reply(&message_context, b"reply response")
                        .unwrap();
                }
            }
            Ok(())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    // Service that can be used for testing a Admin service's functionality
    struct MockAdminService {
        service_id: String,
        service_type: String,
        network_sender: Option<Box<dyn ServiceNetworkSender>>,
    }

    impl MockAdminService {
        pub fn new() -> Self {
            MockAdminService {
                service_id: "mock_service".to_string(),
                service_type: "mock".to_string(),
                network_sender: None,
            }
        }
    }

    impl ServiceInstance for MockAdminService {
        /// This service's id
        fn service_id(&self) -> &str {
            &self.service_id
        }

        /// This service's message family
        fn service_type(&self) -> &str {
            &self.service_type
        }

        /// Starts the service
        fn start(
            &mut self,
            service_registry: &dyn ServiceNetworkRegistry,
        ) -> Result<(), ServiceStartError> {
            let network_sender = service_registry.connect(self.service_id())?;
            self.network_sender = Some(network_sender);
            Ok(())
        }

        /// Stops the service
        fn stop(
            &mut self,
            service_registry: &dyn ServiceNetworkRegistry,
        ) -> Result<(), ServiceStopError> {
            service_registry.disconnect(self.service_id())?;
            Ok(())
        }

        /// Clean-up any resources before the service is removed.
        /// Consumes the service (which, given the use of dyn traits,
        /// this must take a boxed Service instance).
        fn destroy(self: Box<Self>) -> Result<(), ServiceDestroyError> {
            Ok(())
        }

        fn purge(&mut self) -> Result<(), crate::error::InternalError> {
            unimplemented!()
        }

        // for send and send_and_await the handle_message returns the bytes of an admin direct
        // message so it can choose which circuit the message is sent over
        fn handle_message(
            &self,
            message_bytes: &[u8],
            message_context: &ServiceMessageContext,
        ) -> Result<(), ServiceError> {
            if message_bytes == b"send" {
                if let Some(network_sender) = &self.network_sender {
                    network_sender
                        .send(&message_context.sender, b"send_response")
                        .unwrap();
                }
            } else if message_bytes == b"send_and_await" {
                if let Some(network_sender) = &self.network_sender {
                    let response = network_sender
                        .send_and_await(&message_context.sender, b"waiting for response")
                        .unwrap();
                    assert_eq!(response, b"respond to waiting");
                }
            } else if message_bytes == b"reply" {
                if let Some(network_sender) = &self.network_sender {
                    network_sender
                        .reply(&message_context, b"reply response")
                        .unwrap();
                }
            }
            Ok(())
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn create_circuit_direct_msg(payload: Vec<u8>) -> Result<Vec<u8>, protobuf::ProtobufError> {
        let mut direct_response = CircuitDirectMessage::new();
        direct_response.set_recipient("mock_service".to_string());
        direct_response.set_sender("service_a".to_string());
        direct_response.set_circuit("alpha".to_string());
        direct_response.set_payload(payload);
        let bytes = direct_response.write_to_bytes().unwrap();

        let msg = create_message(bytes, CircuitMessageType::CIRCUIT_DIRECT_MESSAGE)?;
        Ok(msg)
    }

    // this message routes back to the mock service so the message can be send and handled by the
    // same service during send_and_await and reply
    fn create_circuit_direct_msg_with_correlation_id(
        payload: Vec<u8>,
        correlation_id: String,
    ) -> Result<Vec<u8>, protobuf::ProtobufError> {
        let mut direct_response = CircuitDirectMessage::new();
        direct_response.set_recipient("mock_service".to_string());
        direct_response.set_sender("mock_service".to_string());
        direct_response.set_circuit("alpha".to_string());
        direct_response.set_correlation_id(correlation_id);
        direct_response.set_payload(payload);
        let bytes = direct_response.write_to_bytes().unwrap();

        let msg = create_message(bytes, CircuitMessageType::CIRCUIT_DIRECT_MESSAGE)?;
        Ok(msg)
    }

    fn create_admin_direct_msg(payload: Vec<u8>) -> Result<Vec<u8>, protobuf::ProtobufError> {
        let mut direct_response = AdminDirectMessage::new();
        direct_response.set_recipient("mock_service".to_string());
        direct_response.set_sender("service_a".to_string());
        direct_response.set_circuit("admin".to_string());
        direct_response.set_payload(payload);
        let bytes = direct_response.write_to_bytes().unwrap();

        let msg = create_message(bytes, CircuitMessageType::ADMIN_DIRECT_MESSAGE)?;
        Ok(msg)
    }

    // this message routes back to the mock service so the message can be send and handled by the
    // same service during send_and_await and reply
    fn create_admin_direct_msg_with_correlation_id(
        payload: Vec<u8>,
        correlation_id: String,
    ) -> Result<Vec<u8>, protobuf::ProtobufError> {
        let mut direct_response = AdminDirectMessage::new();
        direct_response.set_recipient("mock_service".to_string());
        direct_response.set_sender("mock_service".to_string());
        direct_response.set_circuit("admin".to_string());
        direct_response.set_correlation_id(correlation_id);
        direct_response.set_payload(payload);
        let bytes = direct_response.write_to_bytes().unwrap();

        let msg = create_message(bytes, CircuitMessageType::ADMIN_DIRECT_MESSAGE)?;
        Ok(msg)
    }

    fn create_service_connect_response(
        correlation_id: String,
        circuit: String,
    ) -> Result<Vec<u8>, protobuf::ProtobufError> {
        let mut response = ServiceConnectResponse::new();
        response.set_circuit(circuit);
        response.set_service_id("mock_service".to_string());
        response.set_status(ServiceConnectResponse_Status::OK);
        response.set_correlation_id(correlation_id);
        let bytes = response.write_to_bytes().unwrap();

        let msg = create_message(bytes, CircuitMessageType::SERVICE_CONNECT_RESPONSE)?;
        Ok(msg)
    }

    fn create_service_disconnect_response(
        correlation_id: String,
        circuit: String,
    ) -> Result<Vec<u8>, protobuf::ProtobufError> {
        let mut response = ServiceDisconnectResponse::new();
        response.set_circuit(circuit);
        response.set_service_id("mock_service".to_string());
        response.set_status(ServiceDisconnectResponse_Status::OK);
        response.set_correlation_id(correlation_id);
        let bytes = response.write_to_bytes().unwrap();

        let msg = create_message(bytes, CircuitMessageType::SERVICE_DISCONNECT_RESPONSE)?;
        Ok(msg)
    }

    fn get_service_connect(network_msg_bytes: Vec<u8>) -> ServiceConnectRequest {
        let network_msg: NetworkMessage = Message::parse_from_bytes(&network_msg_bytes).unwrap();
        let circuit_msg: CircuitMessage =
            Message::parse_from_bytes(network_msg.get_payload()).unwrap();
        let request: ServiceConnectRequest =
            Message::parse_from_bytes(circuit_msg.get_payload()).unwrap();
        request
    }

    fn get_service_disconnect(network_msg_bytes: Vec<u8>) -> ServiceDisconnectRequest {
        let network_msg: NetworkMessage = Message::parse_from_bytes(&network_msg_bytes).unwrap();
        let circuit_msg: CircuitMessage =
            Message::parse_from_bytes(network_msg.get_payload()).unwrap();
        let request: ServiceDisconnectRequest =
            Message::parse_from_bytes(circuit_msg.get_payload()).unwrap();
        request
    }

    fn get_circuit_direct_msg(network_msg_bytes: Vec<u8>) -> CircuitDirectMessage {
        let network_msg: NetworkMessage = Message::parse_from_bytes(&network_msg_bytes).unwrap();
        let circuit_msg: CircuitMessage =
            Message::parse_from_bytes(network_msg.get_payload()).unwrap();
        let direct_message: CircuitDirectMessage =
            Message::parse_from_bytes(circuit_msg.get_payload()).unwrap();
        direct_message
    }

    fn get_admin_direct_msg(network_msg_bytes: Vec<u8>) -> AdminDirectMessage {
        let network_msg: NetworkMessage = Message::parse_from_bytes(&network_msg_bytes).unwrap();
        let circuit_msg: CircuitMessage =
            Message::parse_from_bytes(network_msg.get_payload()).unwrap();
        let direct_message: AdminDirectMessage =
            Message::parse_from_bytes(circuit_msg.get_payload()).unwrap();
        direct_message
    }
}
