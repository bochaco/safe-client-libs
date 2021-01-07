// Copyright 2020 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::Error;
use bincode::{deserialize, serialize};
use bytes::Bytes;
use futures::{
    future::{join_all, select_all},
    lock::Mutex,
};
use log::{debug, error, info, trace, warn};
use qp2p::{self, Config as QuicP2pConfig, Connection, Endpoint, IncomingMessages, Message as Qp2pMessage, QuicP2p, RecvStream, SendStream};
use sn_data_types::{HandshakeRequest, HandshakeResponse, Keypair, TransferValidated};
use sn_messaging::{Event, Message, MessageId, MsgEnvelope, MsgSender, QueryResponse};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::sync::mpsc::channel;

static NUMBER_OF_RETRIES: usize = 3;
pub static STANDARD_ELDERS_COUNT: usize = 5;

/// Simple map for correlating a response with votes from various elder responses.
type VoteMap = HashMap<[u8; 32], (QueryResponse, usize)>;

// channel for sending result of transfer validation
type TransferValidationSender = Sender<Result<TransferValidated, Error>>;
type QueryResponseSender = Sender<Result<QueryResponse, Error>>;

#[derive(Clone)]
struct ElderStream {
    send_stream: Arc<Mutex<SendStream>>,
    connection: Arc<Mutex<Connection>>,
    listener: Arc<Mutex<NetworkListenerHandle>>,
    socket_addr: SocketAddr,
}

/// JoinHandle for recv stream listener thread
type NetworkListenerHandle = JoinHandle<Result<(), Error>>;
/// Initialises `QuicP2p` instance which can bootstrap to the network, establish
/// connections and send messages to several nodes, as well as await responses from them.
pub struct ConnectionManager {
    keypair: Arc<Keypair>,
    qp2p: QuicP2p,
    elders: Vec<ElderStream>,
    endpoint: Arc<Mutex<Endpoint>>,
    pending_transfer_validations: Arc<Mutex<HashMap<MessageId, TransferValidationSender>>>,
    pending_query_responses: Arc<Mutex<HashMap<(SocketAddr, MessageId), QueryResponseSender>>>,
    notification_sender: UnboundedSender<Error>,
    listener_handle: Option<JoinHandle<Result<(), Error>>>
}

impl ConnectionManager {
    /// Create a new connection manager.
    pub fn new(
        mut config: QuicP2pConfig,
        keypair: Arc<Keypair>,
        notification_sender: UnboundedSender<Error>,
    ) -> Result<Self, Error> {
        config.port = Some(0); // Make sure we always use a random port for client connections.
        let qp2p = QuicP2p::with_config(Some(config), Default::default(), false)?;
        let endpoint = qp2p.new_endpoint()?;
  
        
        Ok(Self {
            keypair,
            qp2p,
            elders: Vec::default(),
            endpoint: Arc::new(Mutex::new(endpoint)),
            pending_transfer_validations: Arc::new(Mutex::new(HashMap::default())),
            pending_query_responses: Arc::new(Mutex::new(HashMap::default())),
            notification_sender,
            listener_handle: None,
        })
    }

    /// Bootstrap to the network maintaining connections to several nodes.
    pub async fn bootstrap(&mut self) -> Result<(), Error> {
        trace!(
            "Trying to bootstrap to the network with public_key: {:?}",
            self.keypair.public_key()
        );

   



        // Bootstrap and send a handshake request to receive
        // the list of Elders we can then connect to
        let elders_addrs = self.bootstrap_and_handshake().await?;

        // Let's now connect to all Elders
        self.connect_to_elders(elders_addrs).await?;

        // And finally set up listeneres on these established connections
        // to monitor incoming messags and route them appropriately
        // self.listener_handle = Some(self.listen_on_endpoint(self.endpoint.clone()).await?);
        
        Ok(())
    }

    /// Send a `Message` to the network without awaiting for a response.
    pub async fn send_cmd(&self, msg: &Message) -> Result<(), Error> {
        info!("Sending command message {:?} w/ id: {:?}", msg, msg.id());
        let msg_bytes = self.serialise_in_envelope(msg)?;

        // Send message to all Elders concurrently
        let mut tasks = Vec::default();
        for elder in &self.elders {
            let msg_bytes_clone = msg_bytes.clone();
            let connection = Arc::clone(&elder.connection);
            let task_handle = tokio::spawn(async move {
                let _ = connection.lock().await.send_bi(msg_bytes_clone).await;
            });
            tasks.push(task_handle);
        }

        // Let's await for all messages to be sent
        let results = join_all(tasks).await;

        let mut failures = 0;
        results.iter().for_each(|res| {
            if res.is_err() {
                failures += 1;
            }
        });

        if failures > 0 {
            error!("Sending the message to {} Elders failed", failures);
        }

        // TODO: return an error if we didn't successfully
        // send it to at least a majority of Elders??

        Ok(())
    }

    /// Send a `Message` to the network without awaiting for a response.
    pub async fn send_transfer_validation(
        &self,
        msg: &Message,
        sender: Sender<Result<TransferValidated, Error>>,
    ) -> Result<(), Error> {
        info!(
            "Sending transfer validation command {:?} w/ id: {:?}",
            msg,
            msg.id()
        );
        let msg_bytes = self.serialise_in_envelope(msg)?;

        let msg_id = msg.id();
        let _ = self
            .pending_transfer_validations
            .lock()
            .await
            .insert(msg_id, sender);

        // Send message to all Elders concurrently
        let mut tasks = Vec::default();
        for elder in &self.elders {
            let msg_bytes_clone = msg_bytes.clone();
            let connection = Arc::clone(&elder.connection);
            let task_handle = tokio::spawn(async move {
                info!("Sending transfer validation to Elder");
                let _ = connection.lock().await.send_bi(msg_bytes_clone).await;
            });
            tasks.push(task_handle);
        }

        // Let's await for all messages to be sent
        let _results = join_all(tasks).await;

        // TODO: return an error if we didn't successfully
        // send it to at least a majority of Elders??

        Ok(())
    }

    /// Send a Query `Message` to the network awaiting for the response.
    pub async fn send_query(&self, msg: &Message) -> Result<QueryResponse, Error> {
        info!("Sending query message {:?} w/ id: {:?}", msg, msg.id());
        let msg_bytes = self.serialise_in_envelope(&msg.clone())?;

        // We send the same message to all Elders concurrently,
        // and we try to find a majority on the responses
        let mut tasks = Vec::default();
        for elder in &self.elders {
            let msg_bytes_clone = msg_bytes.clone();
            let socket_addr = elder.socket_addr;
            let endpoint = self.endpoint.clone();
            // Create a new stream here to not have to worry about filtering replies
            // let connection = Arc::clone(&elder.connection);
            let msg_id = msg.id();

            let pending_query_responses = self
            .pending_query_responses
            .clone();

            let task_handle = tokio::spawn(async move {
                // Retry queries that failed for connection issues
                let mut done_trying = false;
                let mut result = Err(Error::ElderQuery);
                let mut attempts: usize = 1;
                while !done_trying {
                    let msg_bytes_clone = msg_bytes_clone.clone();

                    // How to handle an err here... ? Do we care about one?
                    // let _ = connection.lock().await.send_bi(msg_bytes_clone).await;

                    // let connection = connection.lock().await;

                    let (connection, _) = endpoint.lock().await.connect_to(&socket_addr).await?;
                    // let remote_addr = connection.remote_address();
                    
                    
                    match connection.send_bi(msg_bytes_clone).await {
                        Ok(mut streams) => {
                            // TODO here wait on a channel response insteaad of stream response...?

                            info!("message sent sokay...................");
                            let (sender, mut receiver) = channel::<Result<QueryResponse, Error>>(7);
                            {
                                let _ = pending_query_responses
                                    .lock()
                                    .await
                                    .insert((socket_addr, msg_id), sender);

                                    info!("inserted listener");
                            }
                            info!("Awaiting response in CM from our listener endpoint....");
                            result = match receiver.recv().await {

                                Some(result) => {
                                    debug!("IN THE RESULLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLLT {:?}", result);
                                    match result {
                                    Ok(response) => {
                                        Ok(response)
                                    },
                                    Err(e) => Err(Error::ReceivingQuery)
                                }
                            }
                                ,None => Err(Error::ReceivingQuery)
                            };

    
                            info!("returning from this... why?????????????????????????????????? {:?}", result);
                            // info!("Pendin q length: {:?}", pending_query_responses.)
                            // debug!("Waiting...");
                            // tokio::time::delay_for(tokio::time::Duration::from_secs(20)).await;
                            // debug!("dont wwaiting");
                            // result = match streams.1.next().await {
                            //     Ok(bytes) => Ok(bytes),
                            //     Err(_error) => {
                            //         done_trying = true;
                            //         Err(Error::ReceivingQuery)
                            //     }
                            // }
                        }
                        Err(_error) => result = Err(Error::ReceivingQuery),
                    };

                    debug!(
                        "Try #{:?} @ {:?}. Got back response: {:?}",
                        attempts,
                        socket_addr,
                        &result.is_ok()
                    );

                    if result.is_ok() || attempts > NUMBER_OF_RETRIES {
                        done_trying = true;
                    }

                    attempts += 1;
                }

                result
                // let response = result?;
                // let response = result?;

                // match deserialize(&response) {
                //     Ok(MsgEnvelope { message, .. }) => Ok(message),
                //     Err(e) => {
                //         let err_msg = format!("Unexpected deserialisation error: {:?}", e);
                //         error!("{}", err_msg);
                //         Err(Error::from(e))
                //     }
                // }
            });

            tasks.push(task_handle);
        }

        // Let's figure out what's the value which is in the majority of responses obtained
        let mut vote_map = VoteMap::default();
        let mut received_errors = 0;

        // TODO: make threshold dynamic based upon known elders
        let threshold: usize = (self.elders.len() as f32 / 2_f32).ceil() as usize;

        trace!("Vote threshold is: {:?}", threshold);
        let mut winner: (Option<QueryResponse>, usize) = (None, threshold);

        // Let's await for all responses
        let mut has_elected_a_response = false;

        let mut todo = tasks;

        while !has_elected_a_response {
            if todo.is_empty() {
                warn!("No more connections to try");
                break;
            }

            let (res, _idx, remaining_futures) = select_all(todo.into_iter()).await;
            todo = remaining_futures;

            if let Ok(res) = res {
                match res {
                    Ok(response) => {
                        trace!("QueryResponse is: {:#?}", response);

                        let key = tiny_keccak::sha3_256(&serialize(&response)?);
                        let (_, counter) = vote_map.entry(key).or_insert((response.clone(), 0));
                        *counter += 1;

                        // First, see if this latest response brings us above the threshold for any response
                        if *counter > threshold {
                            trace!("Enough votes to be above response threshold");

                            winner = (Some(response), *counter);
                            has_elected_a_response = true;
                        }
                    }
                    _ => {
                        warn!("Unexpected message in reply to query (retrying): {:?}", res);
                        received_errors += 1;
                    }
                }
            } else if let Err(error) = res {
                error!("Error spawning query task: {:?} ", error);
                received_errors += 1;
            }

            // Second, let's handle no winner on majority responses.
            if !has_elected_a_response {
                winner = self.select_best_of_the_rest_response(
                    winner,
                    threshold,
                    &vote_map,
                    received_errors,
                    &mut has_elected_a_response,
                )?;
            }
        }

        trace!(
            "Response obtained after querying {} nodes: {:?}",
            winner.1,
            winner.0
        );

        winner.0.ok_or(Error::NoResponse)
    }

    /// Choose the best response when no single responses passes the threshold
    fn select_best_of_the_rest_response(
        &self,
        current_winner: (Option<QueryResponse>, usize),
        threshold: usize,
        vote_map: &VoteMap,
        received_errors: usize,
        has_elected_a_response: &mut bool,
    ) -> Result<(Option<QueryResponse>, usize), Error> {
        trace!("No response selected yet, checking if fallback needed");
        let mut number_of_responses = 0;
        let mut most_popular_response = current_winner;

        for (_, (message, votes)) in vote_map.iter() {
            number_of_responses += votes;
            trace!("Number of votes cast :{:?}", number_of_responses);

            number_of_responses += received_errors;

            trace!(
                "Total number of responses (votes and errors) :{:?}",
                number_of_responses
            );

            if most_popular_response.0 == None {
                most_popular_response = (Some(message.clone()), *votes);
            }

            if votes > &most_popular_response.1 {
                trace!("Reselecting winner, with {:?} votes: {:?}", votes, message);

                most_popular_response = (Some(message.clone()), *votes)
            } else {
                // TODO: check w/ farming we get a proper history returned w /matching responses.
                if let QueryResponse::GetHistory(Ok(history)) = &message {
                    // if we're not more popular but in simu payout mode, check if we have more history...
                    if cfg!(feature = "simulated-payouts") && votes == &most_popular_response.1 {
                        if let Some(QueryResponse::GetHistory(res)) = &most_popular_response.0 {
                            if let Ok(popular_history) = res {
                                if history.len() > popular_history.len() {
                                    trace!("GetHistory response received in Simulated Payouts... choosing longest history. {:?}", history);
                                    most_popular_response = (Some(message.clone()), *votes)
                                }
                            }
                        }
                    }
                }
            }
        }

        if number_of_responses > threshold {
            trace!("No clear response above the threshold, so choosing most popular response with: {:?} votes: {:?}", most_popular_response.1, most_popular_response.0);

            *has_elected_a_response = true;
        }

        Ok(most_popular_response)
    }

    // Private helpers

    // Put a `Message` in an envelope so it can be sent to the network
    fn serialise_in_envelope(&self, message: &Message) -> Result<Bytes, Error> {
        trace!("Putting message in envelope: {:?}", message);
        let sign = self.keypair.sign(&serialize(message)?);

        let envelope = MsgEnvelope {
            message: message.clone(),
            origin: MsgSender::client(self.keypair.public_key(), sign)?,
            proxies: Default::default(),
        };

        let bytes = Bytes::from(serialize(&envelope)?);
        Ok(bytes)
    }

    // Bootstrap to the network to obtaining the list of
    // nodes we should establish connections with
    async fn bootstrap_and_handshake(&mut self) -> Result<Vec<SocketAddr>, Error> {
        trace!("Bootstrapping with contacts...");
        let (endpoint, conn, incoming_messages) = self.qp2p.bootstrap().await?;
        self.endpoint = Arc::new(Mutex::new(endpoint));

        self.listener_handle = Some(self.listen_on_endpoint(incoming_messages).await?);


        trace!("Sending handshake request to bootstrapped node...");
        let public_key = self.keypair.public_key();
        let handshake = HandshakeRequest::Bootstrap(public_key);
        let msg = Bytes::from(serialize(&handshake)?);
        let mut streams = conn.send_bi(msg).await?;

        // debug!("Waiting...");
        // tokio::time::delay_for(tokio::time::Duration::from_secs(20)).await;
        // debug!("dont wwaiting");

        
        let response = streams.1.next().await?;

        match deserialize(&response) {
            Ok(HandshakeResponse::Rebootstrap(_elders)) => {
                trace!("HandshakeResponse::Rebootstrap, trying again");
                // TODO: initialise `hard_coded_contacts` with received `elders`.
                unimplemented!();
            }
            Ok(HandshakeResponse::Join(elders)) => {
                trace!("HandshakeResponse::Join Elders: ({:?})", elders);

                // Obtain the addresses of the Elders
                let elders_addrs = elders.into_iter().map(|(_xor_name, ci)| ci).collect();
                Ok(elders_addrs)
            }
            Ok(_msg) => Err(Error::UnexpectedMessageOnJoin),
            Err(e) => Err(Error::from(e)),
        }
    }

    pub fn number_of_connected_elders(&self) -> usize {
        self.elders.len()
    }

    /// Connect and bootstrap to one specific elder
    async fn connect_to_elder(
        endpoint: Arc<Mutex<Endpoint>>,
        peer_addr: SocketAddr,
        keypair: Arc<Keypair>,
    ) -> Result<
        (
            Arc<Mutex<SendStream>>,
            Arc<Mutex<Connection>>,
            RecvStream,
            SocketAddr,
        ),
        Error,
    > {
        let endpoint = endpoint.lock().await;

        info!("....................ELDER CONNECTION ENDPOINT ISSS {:?}", endpoint.socket_addr().await);

        let (connection, _incoming_messages) = endpoint.connect_to(&peer_addr).await?;

        let handshake = HandshakeRequest::Join(keypair.public_key());
        let msg = Bytes::from(serialize(&handshake)?);
        let (send_stream, recv_stream) = connection.send_bi(msg).await?;
        Ok((
            Arc::new(Mutex::new(send_stream)),
            Arc::new(Mutex::new(connection)),
            recv_stream,
            peer_addr,
        ))
    }

    // Connect to a set of Elders nodes which will be
    // the receipients of our messages on the network.
    async fn connect_to_elders(&mut self, elders_addrs: Vec<SocketAddr>) -> Result<(), Error> {
        // Connect to all Elders concurrently
        // We spawn a task per each node to connect to
        let mut tasks = Vec::default();

        for peer_addr in elders_addrs {
            let keypair = self.keypair.clone();

            // We use one endpoint for all elders
            let endpoint = Arc::clone(&self.endpoint);

            let task_handle = tokio::spawn(async move {
                let mut done_trying = false;
                let mut result = Err(Error::ElderConnection);
                let mut attempts: usize = 1;
                while !done_trying {
                    let endpoint = Arc::clone(&endpoint);
                    let keypair = keypair.clone();
                    result = Self::connect_to_elder(endpoint, peer_addr, keypair).await;

                    debug!(
                        "Elder conn attempt #{:?} @ {:?} is ok? : {:?}",
                        attempts,
                        peer_addr,
                        result.is_ok()
                    );

                    if result.is_ok() || attempts > NUMBER_OF_RETRIES {
                        done_trying = true;
                    }

                    attempts += 1;
                }

                result
            });
            tasks.push(task_handle);
        }

        // Let's await for them to all successfully connect, or fail if at least one failed

        //TODO: Do we need a timeout here to check sufficient time has passed + or sufficient connections?
        let mut has_sufficent_connections = false;

        let mut todo = tasks;

        while !has_sufficent_connections {
            if todo.is_empty() {
                warn!("No more elder connections to try");
                break;
            }

            let (res, _idx, remaining_futures) = select_all(todo.into_iter()).await;

            if remaining_futures.is_empty() {
                has_sufficent_connections = true;
            }

            todo = remaining_futures;

            if let Ok(elder_result) = res {
                let res = elder_result.map_err(|err| {
                    // elder connection retires already occur above
                    warn!("Failed to connect to Elder @ : {}", err);
                });

                if let Ok((send_stream, connection, recv_stream, socket_addr)) = res {
                    info!("Connected to elder: {:?}", socket_addr);
                    
                    let listener = self.listen_to_receive_stream(recv_stream).await?;
                    // We can now keep this connections in our instance
                    self.elders.push(ElderStream {
                        send_stream,
                        connection,
                        listener: Arc::new(Mutex::new(listener)),
                        socket_addr,
                    });
                }
            }

            // TODO: this will effectively stop driving futures after we get 2...
            // We should still let all progress... just without blocking
            if self.elders.len() >= STANDARD_ELDERS_COUNT {
                has_sufficent_connections = true;
            }

            if self.elders.len() < STANDARD_ELDERS_COUNT {
                warn!("Connected to only {:?} elders.", self.elders.len());
            }

            if self.elders.len() < STANDARD_ELDERS_COUNT - 2 && has_sufficent_connections {
                return Err(Error::InsufficientElderConnections);
            }
        }

        trace!("Connected to {} Elders.", self.elders.len());
        Ok(())
    }

     /// Listen for incoming messages on a connection
     pub async fn listen_on_endpoint(
        &self,
        mut incoming_messages: IncomingMessages,
        // endpoint: Arc<Mutex<Endpoint>>,
    ) -> Result<NetworkListenerHandle, Error> {
        trace!("Adding endpoint listener");

        

        let pending_transfer_validations = Arc::clone(&self.pending_transfer_validations);
        let notifier = self.notification_sender.clone();
        
        // this never ends... sooooo what?
        // let endpoint = endpoint.lock().await;

        // let socket = endpoint.socket_addr().await;

        // debug!("_________________________________________ CLIENT SOCKET ISSSS: {:?}", socket);



        
        let pending_queries = self.pending_query_responses.clone();
        // let mut incoming = endpoint.listen();

        // loop {
        //     incoming = endpoint.listen();
        // }

        // Spawn a thread for all the connections
        let handle = tokio::spawn(async move {
            info!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!Listening for incoming connections via endpoint");


            // this is recv stream used to send challenge response. Send
            while let Some(mut message ) = incoming_messages.next().await {
                // let connecting_peer = messages.remote_addr();
                
                // trace!("*****************************************Listener message received from {:?}", connecting_peer);

                // while  let Some(message) = messages
                // .next()
                // .await {

                    println!("::::::::::::::::::::::::::::::::::::::::::::Waiting for messages...");
                    let _ = if let Qp2pMessage::BiStream {
                        bytes, send, recv, ..
                    } = message
                    {   
                        info!("%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%%");
                        // pending_queries.lock().await.remove()
                        // (bytes, send, recv)
                        // Ok(())

                        panic!("asss")
                    } else {
                        error!("Only bidirectional streams are supported in this example {:?}", message);
                        // panic!("only bistrem plz");

                        let (mut bytes, mut recv) = if let Qp2pMessage::UniStream {
                            bytes, recv, ..
                        } = message {

                            match deserialize::<MsgEnvelope>(&bytes) {
                                // Message::Event {
                                //     event,
                                //     correlation_id, 
                                //     ..
                                // },
                                msg => {
                                    error!("MEssage was receivedd...... {:?}", msg)
                                }
                            }

                            (bytes, recv)

                            // Ok(())

                        }
                        else{
                            // Ok(())
                            panic!("nblaaa")
                            // (bytes, recv)

                        };






                        // bail!("Only bidirectional streams are supported in this example");
                    };
                }


            // }

            info!("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!Elder listener stopped.");

            Ok::<(), Error>(())
        });


        warn!(">>>>>>>>>>>>>>>>>>>> Endpoint listener thread setup ");

        Ok(handle)
    }

    /// Listen for incoming messages via IncomingConnections.
    pub async fn listen_to_receive_stream(
        &self,
        mut receiver: RecvStream,
    ) -> Result<NetworkListenerHandle, Error> {
        trace!("Adding listener");

        let pending_transfer_validations = Arc::clone(&self.pending_transfer_validations);
        let notifier = self.notification_sender.clone();

        // Spawn a thread for all the connections
        let handle = tokio::spawn(async move {
            info!("Listening for incoming stream messages started");

            // this is recv stream used to send challenge response. Send
            while let Ok(bytes) = receiver.next().await {
                trace!("Listener message received");

                match deserialize::<MsgEnvelope>(&bytes) {
                    Ok(envelope) => {
                        debug!("Message received at listener: {:?}", &envelope.message);

                        match envelope.message.clone() {
                            Message::Event {
                                event,
                                correlation_id,
                                ..
                            } => {
                                if let Event::TransferValidated { event, .. } = event {
                                    if let Some(sender) = pending_transfer_validations
                                        .lock()
                                        .await
                                        .get_mut(&correlation_id)
                                    {
                                        info!("Accumulating SignatureShare");
                                        let _ = sender.send(Ok(event)).await;
                                    }
                                }
                            }
                            Message::CmdError {
                                error,
                                correlation_id,
                                ..
                            } => {
                                if let Some(sender) = pending_transfer_validations
                                    .lock()
                                    .await
                                    .get_mut(&correlation_id)
                                {
                                    debug!("Cmd Error was received, sending on channel to caller");
                                    let _ = sender.send(Err(Error::from(error.clone()))).await;
                                };

                                let _ = notifier.send(Error::from(error));
                            }
                            _ => {
                                warn!("another message type received");
                            }
                        }
                    }
                    Err(_) => error!("Error deserializing network message"),
                };
            }

            info!("Receive stream listener stopped.");

            Ok::<(), Error>(())
        });

        Ok(handle)
    }
}
