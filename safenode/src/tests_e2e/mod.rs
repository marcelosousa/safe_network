// Copyright 2023 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

mod setup;

use crate::domain::{
    dbc_genesis::{get_tokens_from_faucet, send},
    wallet::{DepositWallet, VerifyingClient, Wallet},
};

use sn_dbc::Token;

use assert_fs::TempDir;
use eyre::Result;

#[ignore = "Not yet finished."]
#[tokio::test(flavor = "multi_thread")]
async fn spend_is_stored_in_network() -> Result<()> {
    let first_wallet_dir = TempDir::new()?;
    let first_wallet_balance = Token::from_nano(10_000);

    let mut first_wallet = setup::get_wallet(first_wallet_dir.path()).await;
    let client = setup::get_client();
    println!("Getting tokens from the faucet...");
    let tokens =
        get_tokens_from_faucet(first_wallet_balance, first_wallet.address(), &client).await;
    println!("Verifying the transfer from faucet...");
    client.verify(&tokens).await?;
    first_wallet.deposit(vec![tokens]);
    assert_eq!(first_wallet.balance(), first_wallet_balance);
    println!("Tokens deposited to first wallet: {first_wallet_balance}.");

    let second_wallet_balance = Token::from_nano(first_wallet_balance.as_nano() / 2);
    println!("Transferring from first wallet to second wallet: {second_wallet_balance}.");
    let second_wallet_dir = TempDir::new()?;
    let mut second_wallet = setup::get_wallet(second_wallet_dir.path()).await;

    assert_eq!(second_wallet.balance(), Token::zero());

    let tokens = send(
        first_wallet,
        second_wallet_balance,
        second_wallet.address(),
        &client,
    )
    .await;
    println!("Verifying the transfer from first wallet...");
    client.verify(&tokens).await?;
    second_wallet.deposit(vec![tokens]);
    assert_eq!(second_wallet.balance(), second_wallet_balance);
    println!("Tokens deposited to second wallet: {second_wallet_balance}.");

    // The first wallet will have paid fees for the transfer,
    // so it will have less than half the amount left, but we can't
    // know how much exactly, so we just check that it has less than
    // the original amount.
    let first_wallet = setup::get_wallet(first_wallet_dir.path()).await;
    assert!(second_wallet_balance.as_nano() > first_wallet.balance().as_nano());

    Ok(())
}