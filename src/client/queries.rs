// Copyright 2021 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use super::Client;
use crate::{
    config_handler::Config,
    connections::{QueryResult, Session, Signer},
    errors::Error,
};
use crdts::Dot;
use futures::lock::Mutex;
use log::{debug, info, trace, warn};
use rand::rngs::OsRng;
use sn_data_types::{Keypair, PublicKey, SectionElders, Token};
use sn_messaging::{
    client::{Cmd, DataCmd, Message, Query},
    MessageId,
};
use std::{
    path::Path,
    str::FromStr,
    {collections::HashSet, net::SocketAddr, sync::Arc},
};

impl Client {
    /// Send a Query to the network and await a response
    pub(crate) async fn send_query(&self, query: Query) -> Result<QueryResult, Error> {
        debug!("Sending QueryRequest: {:?}", query);
        self.session.send_query(query).await
    }

    // Build and sign Cmd Message Envelope
    pub(crate) async fn create_cmd_message(&self, msg_contents: Cmd) -> Result<Message, Error> {
        let id = MessageId::new();
        trace!("Creating cmd message with id: {:?}", id);

        Ok(Message::Cmd {
            cmd: msg_contents,
            id,
        })
    }
}
