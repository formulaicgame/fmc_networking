use std::{net::SocketAddr, sync::Arc};

use bevy::prelude::*;
use dashmap::DashMap;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{tcp, TcpListener, TcpStream, ToSocketAddrs},
    runtime::Runtime,
    // TODO: Switch to unbounded so sending is not blocked on the server. It was like this, but
    // there was some unknown memory leak. related perhaps
    // https://github.com/rust-lang/futures-rs/issues/2052
    // still leaks though, just less maybe a bevy issue cause the chunk generator task also blows
    // up a little.
    //sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    sync::mpsc::{channel, Receiver, Sender},
    task::JoinHandle,
};

use crate::{
    messages::{AssetRequest, AssetResponse, ClientIdentification, ClientReady, ServerConfig},
    network_message::{ClientBound, NetworkMessage, ServerBound},
    ConnectionId, NetworkData, NetworkPacket, ServerNetworkEvent, SyncChannel, Username,
    MAX_PACKET_LENGTH,
};

struct NewConnection {
    socket: TcpStream,
    username: String,
}

/// An established connection
pub struct ClientConnection {
    username: String,
    id: ConnectionId,
    receive_task: JoinHandle<()>,
    send_task: JoinHandle<()>,
    send_message: Sender<NetworkPacket>,
    addr: SocketAddr,
}

impl ClientConnection {
    pub fn stop(self) {
        self.receive_task.abort();
        self.send_task.abort();
    }
}

impl std::fmt::Debug for ClientConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConnection")
            .field("username", &self.username)
            .field("id", &self.id)
            .field("addr", &self.addr)
            .finish()
    }
}

/// An instance of a [`NetworkServer`] is used to listen for new client connections
/// using [`NetworkServer::listen`]
#[derive(Resource)]
pub struct NetworkServer {
    runtime: Option<Runtime>,
    /// Map of network messages that should be sent as bevy events
    recv_message_map: Arc<DashMap<&'static str, Vec<(ConnectionId, Box<dyn NetworkMessage>)>>>,
    /// Map of served connections
    established_connections: Arc<DashMap<ConnectionId, ClientConnection>>,
    /// Connections that have been verified and should be added to the established_connections map.
    new_connections: SyncChannel<NewConnection>,
    /// Connections that should be disconnected.
    disconnected_connections: SyncChannel<ConnectionId>,
}

impl std::fmt::Debug for NetworkServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "NetworkServer [{} Connected Clients]",
            self.established_connections.len()
        )
    }
}

impl NetworkServer {
    pub(crate) fn new() -> NetworkServer {
        NetworkServer {
            runtime: None,
            recv_message_map: Arc::new(DashMap::new()),
            established_connections: Arc::new(DashMap::new()),
            new_connections: SyncChannel::new(),
            disconnected_connections: SyncChannel::new(),
        }
    }

    /// Start listening for new clients
    ///
    /// ## Note
    /// If you are already listening for new connections, then this will disconnect existing connections first
    pub fn start(
        &mut self,
        addr: impl ToSocketAddrs + Send + 'static,
        server_config: ServerConfig,
    ) {
        self.stop();

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Could not build tokio runtime");

        // Notify of new connection after it's been verified.
        let new_connections = self.new_connections.sender.clone();

        // Listen for new connections at the bind address
        let listen_loop = async move {
            let listener = match TcpListener::bind(addr).await {
                Ok(listener) => listener,
                Err(err) => {
                    error!("Could not bind listen address, Error: {}", err);
                    return;
                }
            };

            loop {
                let (socket, addr) = match listener.accept().await {
                    Ok(v) => v,
                    Err(err) => {
                        error!("Failed to accept connection, Error: {}", err);
                        continue;
                    }
                };

                match socket.set_nodelay(true) {
                    Ok(_) => (),
                    Err(e) => {
                        error!("Could not set nodelay for [{}]: {}", addr, e);
                        continue;
                    }
                }

                tokio::task::spawn(initialize_connection(
                    socket,
                    server_config.clone(),
                    new_connections.clone(),
                ));
            }
        };

        trace!("Started listening");

        runtime.spawn(listen_loop);
        self.runtime = Some(runtime);
    }

    /// Send a message to one client
    #[track_caller]
    pub fn send_one<T: ClientBound>(&self, connection_id: ConnectionId, message: T) {
        let connection = match self.established_connections.get(&connection_id) {
            Some(conn) => conn,
            None => return,
        };

        let packet = NetworkPacket {
            kind: String::from(T::NAME),
            data: Box::new(message),
        };

        connection.send_message.blocking_send(packet).ok();
    }

    /// Send a message to many clients
    #[track_caller]
    pub fn send_many<'a, T: ClientBound + Clone>(
        &self,
        connection_ids: impl IntoIterator<Item = &'a ConnectionId>,
        message: T,
    ) {
        for connection_id in connection_ids {
            let connection = match self.established_connections.get(connection_id) {
                Some(conn) => conn,
                None => return,
            };

            let packet = NetworkPacket {
                kind: String::from(T::NAME),
                data: Box::new(message.clone()),
            };

            connection.send_message.blocking_send(packet).ok();
        }
    }

    /// Broadcast a message to all connected clients
    pub fn broadcast<T: ClientBound + Clone>(&self, message: T) {
        for connection in self.established_connections.iter() {
            let packet = NetworkPacket {
                kind: String::from(T::NAME),
                data: Box::new(message.clone()),
            };

            connection.send_message.blocking_send(packet).ok();
        }
    }

    /// Disconnect all clients and stop listening for new ones
    pub fn stop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }

        for conn in self.established_connections.iter() {
            self.disconnected_connections.sender.send(*conn.key()).ok();
        }

        self.established_connections.clear();
        self.recv_message_map
            .iter_mut()
            .for_each(|mut messages| messages.clear());
        self.new_connections.receiver.try_iter().for_each(|_| ());
    }

    /// Disconnect a client
    pub fn disconnect(&self, connection_id: ConnectionId) {
        self.disconnected_connections
            .sender
            .try_send(connection_id)
            .unwrap();
    }
}

// For use during initialization
async fn read_packet(socket: &mut TcpStream) -> Option<NetworkPacket> {
    let length = match socket.read_u32().await {
        Ok(len) => len as usize,
        _ => return None,
    };

    const MAX_LENGTH: usize = 100;
    if length > MAX_LENGTH {
        error!(
            "Received too large packet from [{}]: {} > {}",
            socket.peer_addr().unwrap(),
            length,
            MAX_LENGTH
        );
        return None;
    }

    let mut buffer = vec![0; length];

    match socket.read_exact(&mut buffer[..length]).await {
        Ok(_) => (),
        Err(err) => {
            error!(
                "Encountered error while reading stream of length {} from [{}]: {}",
                length,
                socket.peer_addr().unwrap(),
                err
            );
            return None;
        }
    }

    let packet: NetworkPacket = match bincode::deserialize(&buffer[..length]) {
        Ok(packet) => packet,
        Err(err) => {
            error!(
                "Failed to decode network packet from [{}]: {}",
                socket.peer_addr().unwrap(),
                err
            );
            return None;
        }
    };
    Some(packet)
}

async fn send_packet(
    mut socket: impl AsyncWriteExt + Unpin,
    packet: NetworkPacket,
    buffer: &mut [u8],
) {
    let size = match bincode::serialized_size(&packet) {
        Ok(size) => size as usize,
        Err(err) => {
            error!("Could not encode packet {:?}: {}", packet, err);
            return;
        }
    };

    match bincode::serialize_into(&mut buffer[0..size], &packet) {
        Ok(_) => (),
        Err(err) => {
            error!(
                "Could not serialize packet into buffer {:?}: {}",
                packet, err
            );
            return;
        }
    };

    match socket.write_u32(size as u32).await {
        Ok(_) => (),
        Err(err) => {
            error!("Could not send packet length: {:?}: {}", size, err);
            return;
        }
    }

    match socket.write_all(&buffer[0..size]).await {
        Ok(_) => (),
        Err(err) => {
            error!("Could not send packet: {:?}: {}", packet, err);
            return;
        }
    }
}

// TODO: This is just a copy of 'recv_task' with all the things that errored removed. Look it over
// and clean it up if necessary.
async fn initialize_connection(
    mut socket: TcpStream,
    server_config: ServerConfig,
    new_connections: crossbeam_channel::Sender<NewConnection>,
) {
    let mut buffer = vec![0; MAX_PACKET_LENGTH];
    let identity: ClientIdentification = match read_packet(&mut socket).await {
        Some(packet) => match packet.data.downcast() {
            Ok(v) => *v,
            Err(_) => return,
        },
        None => return,
    };

    send_packet(
        &mut socket,
        NetworkPacket {
            kind: ServerConfig::NAME.to_owned(),
            data: Box::new(server_config),
        },
        &mut buffer,
    )
    .await;

    // TODO: Client can request assets repeatedly
    while let Some(packet) = read_packet(&mut socket).await {
        if packet.data.downcast_ref::<AssetRequest>().is_some() {
            let asset_archive = std::fs::read("resources/assets.tar").unwrap();
            send_packet(
                &mut socket,
                NetworkPacket {
                    kind: AssetResponse::NAME.to_owned(),
                    data: Box::new(AssetResponse {
                        file: asset_archive,
                    }),
                },
                &mut buffer,
            )
            .await
        } else if packet.data.downcast_ref::<ClientReady>().is_some() {
            if let Err(err) = new_connections.send(NewConnection {
                socket,
                username: identity.name,
            }) {
                error!("Cannot accept new connections, channel closed: {}", err);
                return;
            }
            return;
        } else {
            return;
        }
    }
}

async fn recv_task(
    conn_id: ConnectionId,
    recv_message_map: Arc<DashMap<&'static str, Vec<(ConnectionId, Box<dyn NetworkMessage>)>>>,
    mut read_socket: tcp::OwnedReadHalf,
    disconnected_connections: crossbeam_channel::Sender<ConnectionId>,
) {
    let mut buffer: Vec<u8> = vec![0; MAX_PACKET_LENGTH];

    trace!("Starting receive task for {}", conn_id);

    loop {
        trace!("Listening for length!");

        let length = match read_socket.read_u32().await {
            Ok(len) => len as usize,
            Err(err) => {
                // If we get an EOF here, the connection was broken and we simply report a 'disconnected' signal
                if err.kind() == std::io::ErrorKind::UnexpectedEof {
                    break;
                }

                error!(
                    "Encountered error while reading length [{}]: {}",
                    conn_id, err
                );
                break;
            }
        };

        trace!("Received packet with length: {}", length);

        if length > MAX_PACKET_LENGTH {
            error!(
                "Received too large packet from [{}]: {} > {}",
                conn_id, length, MAX_PACKET_LENGTH
            );
            break;
        }

        match read_socket.read_exact(&mut buffer[..length]).await {
            Ok(_) => (),
            Err(err) => {
                error!(
                    "Encountered error while reading stream of length {} [{}]: {}",
                    length, conn_id, err
                );
                break;
            }
        }

        trace!("Read buffer of length {}", length);

        let packet: NetworkPacket = match bincode::deserialize(&buffer[..length]) {
            Ok(packet) => packet,
            Err(err) => {
                error!(
                    "Failed to decode network packet from [{}]: {}",
                    conn_id, err
                );
                break;
            }
        };

        trace!("Created a network packet");

        match recv_message_map.get_mut(&packet.kind[..]) {
            Some(mut packets) => packets.push((conn_id, packet.data)),
            None => {
                error!(
                    "Could not find existing entries for message kind: {:?}",
                    packet
                );
            }
        }

        debug!("Received new message of length: {}", length);
    }

    match disconnected_connections.send(conn_id) {
        Ok(_) => (),
        Err(_) => {
            error!("Could not send disconnected event; channel is disconnected");
        }
    }
}

async fn send_task(
    mut recv_message: Receiver<NetworkPacket>,
    mut send_socket: tcp::OwnedWriteHalf,
) {
    let mut buffer: Vec<u8> = vec![0; MAX_PACKET_LENGTH];

    while let Some(message) = recv_message.recv().await {
        let size = match bincode::serialized_size(&message) {
            Ok(size) => size as usize,
            Err(err) => {
                error!("Could not encode packet {:?}: {}", message, err);
                continue;
            }
        };

        match bincode::serialize_into(&mut buffer[0..size], &message) {
            Ok(_) => (),
            Err(err) => {
                error!(
                    "Could not serialize packet into buffer {:?}: {}",
                    message, err
                );
                continue;
            }
        };

        match send_socket.write_u32(size as u32).await {
            Ok(_) => (),
            Err(err) => {
                error!("Could not send packet length: {:?}: {}", size, err);
                return;
            }
        }

        match send_socket.write_all(&buffer[0..size]).await {
            Ok(_) => (),
            Err(err) => {
                error!("Could not send packet: {:?}: {}", message, err);
                return;
            }
        }
    }
}

pub(crate) fn handle_connections(
    mut commands: Commands,
    server: Res<NetworkServer>,
    mut network_events: EventWriter<ServerNetworkEvent>,
) {
    for connection in server.new_connections.receiver.try_iter() {
        let addr = connection.socket.peer_addr().unwrap();

        let mut entity_commands = commands.spawn_empty();

        let connection_id = ConnectionId {
            entity: entity_commands.id(),
            addr,
        };
        entity_commands
            .insert(connection_id.clone())
            .insert(Username(connection.username.to_owned()));

        let (read_socket, send_socket) = connection.socket.into_split();

        // TODO: I changed this from an unbounded channel because of some memory issue I couldn't
        // diagnose.
        let (send_message, recv_message) = channel(10);

        server.established_connections.insert(
            connection_id,
            ClientConnection {
                username: connection.username.to_owned(),
                id: connection_id,
                receive_task: server.runtime.as_ref().unwrap().spawn(recv_task(
                    connection_id,
                    server.recv_message_map.clone(),
                    read_socket,
                    server.disconnected_connections.sender.clone(),
                )),
                send_task: server
                    .runtime
                    .as_ref()
                    .unwrap()
                    .spawn(send_task(recv_message, send_socket)),
                send_message,
                addr,
            },
        );

        network_events.send(ServerNetworkEvent::Connected {
            entity: connection_id.entity,
        });
    }
}

// TODO: When you disconnnect is prints a bunch of errors because it still has
// access to the connection even though it's disconnected when trying to send.
//
pub(crate) fn send_disconnection_events(
    server: Res<NetworkServer>,
    mut network_events: EventWriter<ServerNetworkEvent>,
) {
    for disconnected_connection in server.disconnected_connections.receiver.try_iter() {
        let connection = match server
            .established_connections
            .remove(&disconnected_connection)
        {
            Some(conn) => conn.1,
            None => continue,
        };

        connection.stop();

        network_events.send(ServerNetworkEvent::Disconnected {
            entity: disconnected_connection.entity,
        });
    }
}

pub(crate) fn handle_disconnection_events(
    mut commands: Commands,
    mut disconnection_events: EventReader<ServerNetworkEvent>,
) {
    for event in disconnection_events.read() {
        if let ServerNetworkEvent::Disconnected { entity } = event {
            commands.entity(*entity).despawn_recursive();
        }
    }
}

/// A utility trait on [`App`] to easily register [`ServerMessage`]s
pub trait AppNetworkServerMessage {
    /// Register a server message type
    ///
    /// ## Details
    /// This will:
    /// - Add a new event type of [`NetworkData<T>`]
    /// - Register the type for transportation over the wire
    /// - Internal bookkeeping
    fn listen_for_server_message<T: ServerBound>(&mut self) -> &mut Self;
}

impl AppNetworkServerMessage for App {
    fn listen_for_server_message<T: ServerBound>(&mut self) -> &mut Self {
        let server = self.world.get_resource::<NetworkServer>().expect("Could not find `NetworkServer`. Be sure to include the `ServerPlugin` before listening for server messages.");

        debug!("Registered a new ServerMessage: {}", T::NAME);

        assert!(
            !server.recv_message_map.contains_key(T::NAME),
            "Duplicate registration of ServerMessage: {}",
            T::NAME
        );
        server.recv_message_map.insert(T::NAME, Vec::new());
        self.add_event::<NetworkData<T>>();
        self.add_systems(PreUpdate, register_server_message::<T>)
    }
}

fn register_server_message<T>(
    net_res: ResMut<NetworkServer>,
    mut events: EventWriter<NetworkData<T>>,
) where
    T: ServerBound,
{
    let mut messages = match net_res.recv_message_map.get_mut(T::NAME) {
        Some(messages) => messages,
        None => return,
    };

    events.send_batch(
        messages
            .drain(..)
            .flat_map(|(conn, msg)| msg.downcast().map(|msg| NetworkData::new(conn, *msg))),
    );
}
