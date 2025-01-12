// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

pub(crate) mod send_client;
pub(crate) mod verifying_client;

use super::Client;

use crate::domain::wallet::{Error, Result, SendWallet};

use sn_dbc::{Dbc, PublicAddress, Token};

/// A wallet client can be used to send and
/// receive tokens to/from other wallets.
pub struct WalletClient<W: SendWallet> {
    client: Client,
    wallet: W,
}

impl<W: SendWallet> WalletClient<W> {
    /// Create a new wallet client.
    pub fn new(client: Client, wallet: W) -> Self {
        Self { client, wallet }
    }

    /// Send tokens to another wallet.
    pub async fn send(&mut self, amount: Token, to: PublicAddress) -> Result<Dbc> {
        let dbcs = self.wallet.send(vec![(amount, to)], &self.client).await?;
        match &dbcs[..] {
            [info, ..] => Ok(info.dbc.clone()),
            [] => Err(Error::CouldNotSendTokens(
                "No DBCs were returned from the wallet.".into(),
            )),
        }
    }

    /// Return the wallet.
    pub fn into_wallet(self) -> W {
        self.wallet
    }
}
