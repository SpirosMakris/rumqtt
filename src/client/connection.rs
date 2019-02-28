use crate::client::{
    mqttstate::MqttState, network::stream::NetworkStream, prepend::Prepend, Command, Notification, Request, UserHandle,
};
use crate::codec::MqttCodec;
use crate::error::{ConnectError, NetworkError};
use crate::mqttoptions::{ConnectionMethod, MqttOptions, Proxy, ReconnectOptions};
use crossbeam_channel::{self, Sender};
use futures::{
    future,
    stream::{self, poll_fn, SplitStream},
    sync::mpsc::{self, Receiver},
    Async, Future, Poll, Sink, Stream,
};
use mqtt311::Packet;
use std::{cell::RefCell, rc::Rc, thread, time::Duration};
use tokio::codec::Framed;
use tokio::prelude::StreamExt;
use tokio::runtime::current_thread::Runtime;
use tokio::timer::{timeout, Timeout};

pub struct Connection {
    mqtt_state: Rc<RefCell<MqttState>>,
    notification_tx: Sender<Notification>,
    connection_tx: Option<Sender<Result<(), ConnectError>>>,
    connection_count: u32,
    mqttoptions: MqttOptions,
    is_network_enabled: bool,
}

impl Connection {
    /// Takes mqtt options and tries to create initial connection on current thread and handles
    /// connection events in a new thread if the initial connection is successful
    pub fn run(mqttoptions: MqttOptions) -> Result<UserHandle, ConnectError> {
        let (notification_tx, notification_rx) = crossbeam_channel::bounded(mqttoptions.notification_channel_capacity());
        let (request_tx, request_rx) = mpsc::channel::<Request>(mqttoptions.request_channel_capacity());
        let (command_tx, command_rx) = mpsc::channel::<Command>(5);

        let (connection_tx, connection_rx) = crossbeam_channel::bounded(1);
        let reconnect_option = mqttoptions.reconnect_opts();

        // start the network thread to handle all mqtt network io
        thread::spawn(move || {
            let mqtt_state = Rc::new(RefCell::new(MqttState::new(mqttoptions.clone())));
            let mut connection = Connection {
                mqtt_state,
                notification_tx,
                connection_tx: Some(connection_tx),
                connection_count: 0,
                mqttoptions,
                is_network_enabled: true,
            };

            connection.mqtt_eventloop(request_rx, command_rx)
        });

        // return user handle to client to send requests and handle notifications
        let user_handle = UserHandle {
            request_tx,
            command_tx,
            notification_rx,
        };

        match reconnect_option {
            ReconnectOptions::AfterFirstSuccess(_) => connection_rx.recv()??,
            ReconnectOptions::Never => connection_rx.recv()??,
            ReconnectOptions::Always(_) => {
                // read the result but ignore it
                let _ = connection_rx.recv()?;
            }
        }

        Ok(user_handle)
    }

    // NOTE: We need to use same reactor across threads because io resources (framed) will
    //       bind to reactor lazily.
    //       You'll face `reactor gone` error if `framed` is used again with a new recator
    fn mqtt_eventloop(&mut self, request_rx: Receiver<Request>, mut command_rx: Receiver<Command>) {
        let network_request_stream = request_rx.map_err(|_| NetworkError::Blah);
        let mut network_request_stream = network_request_stream.prependable();
        let mut command_stream = self.command_stream(command_rx.by_ref());

        'reconnection: loop {
            let mqtt_connect_future = self.mqtt_connect();
            let timeout = Duration::from_secs(30);
            let (runtime, framed) = match self.connect_timeout(mqtt_connect_future, timeout) {
                Ok(f) => f,
                Err(true) => continue 'reconnection,
                Err(false) => break 'reconnection,
            };

            let (network_sink, network_stream) = framed.split();
            let network_sink = network_sink.sink_map_err(NetworkError::Io);
            let network_reply_stream = self.network_reply_stream(network_stream);

            let network_request_stream = &mut network_request_stream;
            // Insert previous session. If this is the first connect, the buffer in
            // network_request_stream is empty.
            network_request_stream.insert(self.mqtt_state.borrow_mut().handle_reconnection());
            let network_request_stream = self.limit_in_flight_request_stream(network_request_stream);
            let network_request_stream = self.request_stream(network_request_stream);

            let mqtt_future = self.mqtt_future(&mut command_stream,
                                               network_request_stream,
                                               network_reply_stream,
                                               network_sink);

            // let mqtt_future = network_stream.select(command_stream).forward(network_sink);
            match self.mqtt_io(runtime, mqtt_future) {
                Err(true) => continue 'reconnection,
                Err(false) => (),
                Ok(_v) => ()
            };

            if self.should_reconnect_again() { continue 'reconnection } else { break 'reconnection }
        }
    }
    
    /// Makes a blocking mqtt connection an returns framed and reactor
    fn connect_timeout(
        &mut self,
        mqtt_connect_future: impl FramedFuture,
        timeout: Duration,
    ) -> Result<(Runtime, MqttFramed), bool> {
        // mqtt connection
        let mut rt = Runtime::new().unwrap();
        let mqtt_connect_deadline = Timeout::new(mqtt_connect_future, timeout);

        let framed = match rt.block_on(mqtt_connect_deadline) {
            Ok(framed) => {
                info!("Mqtt connection successful!!");
                self.handle_connection_success();
                framed
            }
            Err(e) => {
                error!("Connection error = {:?}", e);
                self.handle_connection_error(e);
                return Err(self.should_reconnect_again());
            }
        };

        Ok((rt, framed))
    }

    fn should_reconnect_again(&self) -> bool {
        let reconnect_options = self.mqttoptions.reconnect_opts();
        let is_disconnecting = self.mqtt_state.clone().borrow().is_disconnecting();

        let reconn_policy_action = match reconnect_options {
            ReconnectOptions::AfterFirstSuccess(time) => {
                let time = Duration::from_secs(time);
                thread::sleep(time);
                true
            }
            ReconnectOptions::Always(time) => {
                let time = Duration::from_secs(time);
                thread::sleep(time);
                true
            }
            ReconnectOptions::Never => false,
        };

        reconn_policy_action && !is_disconnecting
    }

    fn mqtt_io(&mut self, mut runtime: Runtime, mqtt_future: impl Future<Item = (), Error = NetworkError>) -> Result<(), bool> {
        match runtime.block_on(mqtt_future) {
            Err(NetworkError::UserDisconnect) => {
                info!("User commanded for network disconnect");
                self.is_network_enabled = false;
                Err(true)
            }
            Err(NetworkError::UserReconnect) => {
                info!("User commanded for network reconnect");
                self.is_network_enabled = true;
                Err(true)
            }
            Err(NetworkError::NetworkStreamClosed) => {
                let mqtt_state = self.mqtt_state.borrow();
                if mqtt_state.is_disconnecting() {
                    info!("Shutting down gracefully");
                }
                Err(false)
            }
            Err(e) => {
                error!("Event loop returned. Error = {:?}", e);
                Err(false)
            }
            Ok(_v) => {
                warn!("Strange!! Evenloop finished");
                Err(false)
            }
        }
    }

    fn mqtt_future(
        &mut self,
        command_stream: impl Stream<Item = Packet, Error = NetworkError>,
        network_request_stream: impl Stream<Item = Request, Error = NetworkError>,
        network_reply_stream: impl Stream<Item = Request, Error = NetworkError>,
        network_sink: impl PacketSink)
        -> impl Future<Item = (), Error = NetworkError> {
        // check if the network is enabled and create a future
        let mqtt_state = self.mqtt_state.clone();
        let keep_alive = self.mqttoptions.keep_alive();

        // convert a reply request stream to reply packet stream after filtering
        // unnecessary requests
        let network_reply_stream = Timeout::new(network_reply_stream, keep_alive)
                                        .or_else(move |e| {
                                            let mut mqtt_state = mqtt_state.borrow_mut();
                                            handle_stream_timeout_error(e, &mut mqtt_state)
                                        })
                                        .filter(|reply| should_forward_packet(reply))
                                        .and_then(move |packet| future::ok(packet.into()));

        // convert a request request stream to request packet stream after filtering
        // unnecessary requests
        let network_request_stream = network_request_stream
                                        .and_then(move |packet| future::ok(packet.into()));
        let network_stream = network_reply_stream.select(network_request_stream);

        let stream = if self.is_network_enabled {
            EitherStream::A(command_stream.select(network_stream))
        } else {
            EitherStream::B(command_stream)
        };

        let throttled_stream = self.rate_limited_network_stream(stream);
        throttled_stream.forward(network_sink).map(|(_, _)| ())
    }

    fn handle_connection_success(&mut self) {
        self.connection_count += 1;

        if self.connection_count == 1 {
            let connection_tx = self.connection_tx.take().unwrap();
            connection_tx.send(Ok(())).unwrap();
        }
    }

    fn handle_connection_error(&mut self, error: timeout::Error<ConnectError>) {
        self.connection_count += 1;

        let error = match error.into_inner() {
            Some(e) => Err(e),
            None => Err(ConnectError::Timeout),
        };

        if self.connection_count == 1 {
            let connection_tx = self.connection_tx.take().unwrap();
            connection_tx.send(error).unwrap();
        }
    }

    /// Resolves dns with blocking API and composes a future
    /// which makes a new tcp or tls connection to the broker.
    /// Note that this doesn't actual connect to the broker
    fn tcp_connect_future(&self) -> impl Future<Item = MqttFramed, Error = ConnectError> {
        let (host, port) = self.mqttoptions.broker_address();
        let connection_method = self.mqttoptions.connection_method();
        let proxy = self.mqttoptions.proxy();

        let builder = NetworkStream::builder();

        let builder = match connection_method {
            ConnectionMethod::Tls(ca, Some((cert, key))) => builder.add_certificate_authority(&ca).add_client_auth(&cert, &key),
            ConnectionMethod::Tls(ca, None) => builder.add_certificate_authority(&ca),
            ConnectionMethod::Tcp => builder,
        };

        let builder = match proxy {
            Proxy::None => builder,
            Proxy::HttpConnect(proxy_host, proxy_port, key, expiry) => {
                let id = self.mqttoptions.client_id();
                builder.set_http_proxy(&id, &proxy_host, proxy_port, &key, expiry)
            }
        };

        builder.connect(&host, port)
    }

    /// Composes a new future which is a combination of tcp connect + mqtt handshake
    fn mqtt_connect(&self) -> impl Future<Item = MqttFramed, Error = ConnectError> {
        let mqtt_state = self.mqtt_state.clone();
        let tcp_connect_future = self.tcp_connect_future();
        let connect_packet = self.mqtt_state.borrow_mut().handle_outgoing_connect().unwrap();

        tcp_connect_future
            .and_then(move |framed| {
                let packet = Packet::Connect(connect_packet);
                framed.send(packet).map_err(ConnectError::Io)
            })
            .and_then(|framed| framed.into_future().map_err(|(err, _framed)| ConnectError::Io(err)))
            .and_then(move |(response, framed)| {
                info!("Mqtt connect response = {:?}", response);
                let mut mqtt_state = mqtt_state.borrow_mut();
                check_and_validate_connack(response, framed, &mut mqtt_state)
            })
    }

    /// Handles all incoming network packets (including sending notifications to user over crossbeam
    /// channel) and creates a stream of packets to send on network
    fn network_reply_stream(&self, network_stream: SplitStream<MqttFramed>) -> impl RequestStream {
        let mqtt_state = self.mqtt_state.clone();
        let notification_tx = self.notification_tx.clone();
        let network_stream = network_stream
            .map_err(NetworkError::Io)
            .and_then(move |packet| {
                debug!("Incoming packet = {:?}", packet_info(&packet));
                let reply = mqtt_state.borrow_mut().handle_incoming_mqtt_packet(packet);
                future::result(reply)
            })
            .and_then(move |(notification, reply)| {
                handle_notification(notification, &notification_tx);
                future::ok(reply)
            })
            .filter(|reply| should_forward_packet(reply))
            .and_then(future::ok);

        network_stream.chain(stream::once(Err(NetworkError::NetworkStreamClosed)))
    }

    /// Handles all incoming user and session requests and creates a stream of packets to send
    /// on network
    /// All the remaining packets in the last session (when cleansession = false) will be prepended
    /// to user request stream to ensure that they are handled first. This cleanly handles last
    /// session stray (even if disconnect happens while sending last session data)because we always
    /// get back this stream from reactor after disconnection.
    fn request_stream(&mut self, request: impl RequestStream) -> impl RequestStream {
        // process user requests and convert them to network packets
        let mqtt_state = self.mqtt_state.clone();
        let request_stream = request
            .map_err(|e| {
                error!("User request error = {:?}", e);
                NetworkError::Blah
            })
            .inspect(|request| {
                debug!("{}", request_info(request));
            })
            .and_then(move |userrequest| {
                let mut mqtt_state = mqtt_state.borrow_mut();
                validate_userrequest(userrequest, &mut mqtt_state)
            });

        let mqtt_state = self.mqtt_state.clone();
        request_stream.and_then(move |packet: Packet| {
            let mut mqtt_state = mqtt_state.borrow_mut();
            mqtt_state.handle_outgoing_mqtt_packet(packet)
        })
    }

    // Apply outgoing queue limit (in flights) by answering stream poll with not ready if queue is full
    // by returning NotReady.
    fn limit_in_flight_request_stream(&self, requests: impl RequestStream) -> impl RequestStream {
        let mqtt_state = self.mqtt_state.clone();
        let in_flight = self.mqttoptions.in_flight();
        let mut stream = requests.peekable();
        poll_fn(move || -> Poll<Option<Request>, NetworkError> {
            if mqtt_state.borrow().publish_queue_len() >= in_flight {
                match stream.peek() {
                    Err(_) => stream.poll(),
                    _ => Ok(Async::NotReady)
                }
            } else {
                stream.poll()
            }
        })
    }

    // Apply rate limit if configured
    fn rate_limited_network_stream(&mut self, request: impl PacketStream) -> impl PacketStream {
        if let Some(rate) = self.mqttoptions.outgoing_ratelimit() {
            let duration = Duration::from_nanos(1_000_000_000 / rate);
            let throttled = request.throttle(duration)
                .map_err(|_| NetworkError::ThrottleError);
            EitherStream::A(throttled)
        } else {
            EitherStream::B(request)
        }
    }

    fn command_stream<'a>(&mut self, commands: &'a mut mpsc::Receiver<Command>) -> impl PacketStream + 'a {
        // process user commands and raise appropriate error to the event loop
        commands
            .or_else(|_err| Err(NetworkError::Blah))
            .and_then(|usercommand| match usercommand {
                Command::Pause => Err(NetworkError::UserDisconnect),
                Command::Resume => Err(NetworkError::UserReconnect),
            })
    }
}

fn handle_stream_timeout_error(
    error: timeout::Error<NetworkError>,
    mqtt_state: &mut MqttState)
    -> impl Future<Item = Request, Error = NetworkError> {
    // check if a ping to the broker is necessary
    let out = mqtt_state.handle_outgoing_mqtt_packet(Packet::Pingreq);
    future::err(error).or_else(move |e| {
        if e.is_elapsed() {
            out
        } else {
            Err(e.into_inner().unwrap())
        }
    })
}

fn validate_userrequest(userrequest: Request, mqtt_state: &mut MqttState) -> impl PacketFuture {
    match userrequest {
        Request::Reconnect(mqttoptions) => {
            mqtt_state.opts = mqttoptions;
            future::err(NetworkError::UserReconnect)
        }
        _ => future::ok(userrequest.into()),
    }
}

fn handle_notification(notification: Notification, notification_tx: &Sender<Notification>) {
    match notification {
        Notification::None => (),
        _ => match notification_tx.try_send(notification) {
            Ok(()) => (),
            Err(e) => error!("Notification send failed. Error = {:?}", e),
        },
    }
}

/// Checks if incoming packet is mqtt connack packet. Useful after mqtt
/// connect when we are waiting for connack but not any other packet.
fn check_and_validate_connack(packet: Option<Packet>, framed: MqttFramed, mqtt_state: &mut MqttState) -> impl FramedFuture {
    match packet {
        Some(Packet::Connack(connack)) => match mqtt_state.handle_incoming_connack(connack) {
            Err(err) => future::err(err),
            _ => future::ok(framed),
        },
        Some(packet) => future::err(ConnectError::NotConnackPacket(packet)),
        None => future::err(ConnectError::NoResponse),
    }
}

fn should_forward_packet(reply: &Request) -> bool {
    match reply {
        Request::None => false,
        _ => true,
    }
}

fn packet_info(packet: &Packet) -> String {
    match packet {
        Packet::Publish(p) => format!(
            "topic = {}, \
             qos = {:?}, \
             pkid = {:?}, \
             payload size = {:?} bytes",
            p.topic_name,
            p.qos,
            p.pkid,
            p.payload.len()
        ),

        _ => format!("{:?}", packet),
    }
}

fn request_info(packet: &Request) -> String {
    match packet {
        Request::Publish(p) => format!(
            "topic = {}, \
             qos = {:?}, \
             pkid = {:?}, \
             payload size = {:?} bytes",
            p.topic_name,
            p.qos,
            p.pkid,
            p.payload.len()
        ),

        _ => format!("{:?}", packet),
    }
}

impl From<Request> for Packet {
    fn from(item: Request) -> Self {
        match item {
            Request::Publish(publish) => Packet::Publish(publish),
            Request::PubAck(pkid) => Packet::Puback(pkid),
            Request::PubRec(pkid) => Packet::Pubrec(pkid),
            Request::PubRel(pkid) => Packet::Pubrel(pkid),
            Request::PubComp(pkid) => Packet::Pubcomp(pkid),
            Request::Ping => Packet::Pingreq,
            Request::Disconnect => Packet::Disconnect,
            Request::Subscribe(subscribe) => Packet::Subscribe(subscribe),
            Request::Unsubscribe(unsubscribe) => Packet::Unsubscribe(unsubscribe),
            _ => unimplemented!(),
        }
    }
}


type MqttFramed = Framed<NetworkStream, MqttCodec>;

trait PacketStream: Stream<Item = Packet, Error = NetworkError> {}
impl<T> PacketStream for T where T: Stream<Item = Packet, Error = NetworkError> {}

trait PacketSink: Sink<SinkItem = Packet, SinkError = NetworkError> {}
impl<T> PacketSink for T where T: Sink<SinkItem = Packet, SinkError = NetworkError> {}

trait CommandStream: Stream<Item = Command, Error = NetworkError> {}
impl<T> CommandStream for T where T: Stream<Item = Command, Error = NetworkError> {}

trait RequestStream: Stream<Item = Request, Error = NetworkError> {}
impl<T> RequestStream for T where T: Stream<Item = Request, Error = NetworkError> {}

trait PacketFuture: Future<Item = Packet, Error = NetworkError> {}
impl<T> PacketFuture for T where T: Future<Item = Packet, Error = NetworkError> {}

trait RequestFuture: Future<Item = Request, Error = NetworkError> {}
impl<T> RequestFuture for T where T: Future<Item = Request, Error = NetworkError> {}

trait FramedFuture: Future<Item = MqttFramed, Error = ConnectError> {}
impl<T> FramedFuture for T where T: Future<Item = MqttFramed, Error = ConnectError> {}

// TODO: Remove if impl Either for Stream is backported to futures 0.1
// See https://github.com/rust-lang-nursery/futures-rs/issues/614
#[derive(Debug)]
pub enum EitherStream<A, B> {
    /// First branch of the type
    A(A),
    /// Second branch of the type
    B(B),
}

impl<A, B> Stream for EitherStream<A, B>
     where A: Stream,
           B: Stream<Item = A::Item, Error = A::Error>
 {
     type Item = A::Item;
     type Error = A::Error;

      fn poll(&mut self) -> Poll<Option<A::Item>, A::Error> {
         match *self {
             EitherStream::A(ref mut a) => a.poll(),
             EitherStream::B(ref mut b) => b.poll(),
         }
     }
 }

// fn print_last_session_state(
//     prepend: &mut Prepend<impl RequestStream>,
//     state: &MqttState) {
//         let last_session_data = prepend.session.iter().map(|request| {
//             match request {
//                 Request::Publish(publish) => publish.pkid,
//                 _ => None
//             }
//         }).collect::<VecDeque<Option<PacketIdentifier>>>();

//         debug!("{:?}", last_session_data);
//         debug!("{:?}", state.publish_queue_len())
// }
