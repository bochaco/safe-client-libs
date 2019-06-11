// Copyright 2018 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

use crate::client::mock::routing::unlimited_muts;
use crate::config_handler::Config;
use routing::{AccountInfo, ClientError};
use rust_sodium::crypto::sign;
use safe_nd::{AppPermissions, Coins, Error};
use std::collections::{BTreeMap, VecDeque};
use std::str::FromStr;

pub const DEFAULT_MAX_MUTATIONS: u64 = 1000;
pub const DEFAULT_MAX_CREDITS: usize = 100;
pub const DEFAULT_COINS: &str = "100";

#[derive(Deserialize, Serialize)]
pub struct Credit {
    amount: Coins,
    transaction_id: u64, // TODO: use Uuid
}

#[derive(Deserialize, Serialize)]
pub struct Account {
    account_info: AccountInfo,
    auth_keys: BTreeMap<sign::PublicKey, AppPermissions>,
    version: u64,
    config: Config,
    balance: Coins,
    credits: VecDeque<Credit>,
}

impl Account {
    pub fn new(config: Config) -> Self {
        Account {
            account_info: AccountInfo {
                mutations_done: 0,
                mutations_available: DEFAULT_MAX_MUTATIONS,
            },
            auth_keys: Default::default(),
            version: 0,
            credits: VecDeque::with_capacity(DEFAULT_MAX_CREDITS),
            balance: unwrap!(Coins::from_str(DEFAULT_COINS)),
            config,
        }
    }

    pub fn credit_balance(&mut self, amount: Coins, transaction_id: u64) -> Result<(), Error> {
        if let Some(new_balance) = self.balance.checked_sub(amount) {
            self.balance = new_balance;
            self.add_transaction(amount, transaction_id);
            Ok(())
        } else {
            Err(Error::InsufficientBalance)
        }
    }

    pub fn debit_balance(&mut self, amount: Coins) -> Result<(), Error> {
        if let Some(new_balance) = self.balance.checked_add(amount) {
            self.balance = new_balance;
            Ok(())
        } else {
            Err(Error::ExcessiveValue)
        }
    }

    pub fn balance(&self) -> Coins {
        self.balance
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn find_transaction(&self, transaction_id: u64) -> Option<Coins> {
        self.credits
            .iter()
            .find(|c| c.transaction_id == transaction_id)
            .map(|c| c.amount)
    }

    fn add_transaction(&mut self, amount: Coins, transaction_id: u64) {
        if self.credits.len() == DEFAULT_MAX_CREDITS {
            let _ = self.credits.pop_back();
        }
        let credit = Credit {
            amount,
            transaction_id,
        };
        self.credits.push_front(credit);
    }

    pub fn account_info(&self) -> &AccountInfo {
        &self.account_info
    }

    // Insert new auth key and bump the version. Returns false if the given version
    // is not one more than the current version.
    pub fn ins_auth_key(
        &mut self,
        key: sign::PublicKey,
        permissions: AppPermissions,
        version: u64,
    ) -> Result<(), ClientError> {
        self.validate_version(version)?;

        let _ = self.auth_keys.insert(key, permissions);
        self.version = version;
        Ok(())
    }

    // Remove the auth key and bump the version. Returns false if the given version
    // is not one more than the current version.
    pub fn del_auth_key(&mut self, key: &sign::PublicKey, version: u64) -> Result<(), ClientError> {
        self.validate_version(version)?;

        if self.auth_keys.remove(key).is_some() {
            self.version = version;
            Ok(())
        } else {
            Err(ClientError::NoSuchKey)
        }
    }

    pub fn auth_keys(&self) -> &BTreeMap<sign::PublicKey, AppPermissions> {
        &self.auth_keys
    }

    pub fn increment_mutations_counter(&mut self) {
        self.account_info.mutations_done += 1;
        // Decrement mutations available, unless we're at 0 and we have unlimited mutations.
        let unlimited_muts = unlimited_muts(&self.config);
        if self.account_info.mutations_available > 0 && !unlimited_muts {
            self.account_info.mutations_available -= 1;
        }
        self.version += 1;
    }

    fn validate_version(&self, version: u64) -> Result<(), ClientError> {
        if version == self.version + 1 {
            Ok(())
        } else {
            Err(ClientError::InvalidSuccessor(self.version))
        }
    }
}
