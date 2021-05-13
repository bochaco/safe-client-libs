// Copyright 2021 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use anyhow::{Context, Result};
use sn_client::Client;
use sn_data_types::BlobAddress;
use sn_url::{SafeContentType, SafeUrl, DEFAULT_XORURL_BASE};
use std::io::{stdout, Write};
use xor_name::{XorName, XOR_NAME_LEN};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    println!("Creating a Client...");
    let bootstrap_contacts = vec!["127.0.0.1:56923".parse()?].into_iter().collect();

    let client = Client::new(None, None, Some(bootstrap_contacts)).await?;

    let pk = client.public_key().await;
    println!("Client Public Key: {}", pk);

    let raw_data = b"Hello Safe World TWO!";
    let address = client.store_public_blob(raw_data).await?;
    let xorurl = SafeUrl::encode_blob(*address.name(), SafeContentType::Raw, DEFAULT_XORURL_BASE)?;
    println!("Blob stored at xorurl: {}", xorurl);

    //let address = BlobAddress::Public(SafeUrl::from_xorurl("safe://hyryyyyk16i9bnf6qyjqm46yyrw3enyu7yiz4cxkda7giktdyo7e6pcnyhc")?.xorname());
    //println!("Blob stored at {:?}:", address);

    let data = client.read_blob(address, None, None).await?;
    println!("Blob read from {:?}:", address);
    stdout()
        .write_all(&data)
        .context("Failed to print out the content of the file")?;

    Ok(())
}
