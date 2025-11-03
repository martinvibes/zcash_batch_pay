use std::fmt::Error;
use std::num::NonZeroU32;
use std::process::CommandArgs;

use bip0039::{English, Mnemonic};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{IntoRequest, Response};
use zcash_client_backend::data_api::wallet::{self, ConfirmationsPolicy};
use zcash_client_backend::data_api::{
    Account, AccountBirthday, AccountPurpose, InputSource, WalletRead, WalletSummary, WalletWrite,
};
use zcash_client_backend::data_api::{WalletCommitmentTrees, Zip32Derivation};
use zcash_client_backend::keys::{UnifiedFullViewingKey, UnifiedSpendingKey};
use zcash_client_backend::proto::service::{self, BlockId};
use zcash_client_backend::proto::service::{
    ChainSpec, compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_client_memory::MemoryWalletDb;
use zcash_protocol::consensus::{Parameters, TEST_NETWORK};
use zip32::AccountId;

mod client;

pub struct SpendingAccount<P: Parameters> {
    memory_db: MemoryWalletDb<P>,
    client: CompactTxStreamerClient<Channel>,
}

impl<P: Parameters> SpendingAccount<P> {
    pub fn new(
        memory_db: MemoryWalletDb<P>,
        client: CompactTxStreamerClient<Channel>,
    ) -> SpendingAccount<P> {
        SpendingAccount { memory_db, client }
    }

    pub async fn init(
        &mut self,
        seed_phrase: &str,
        birthday: u64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mnemonic = Mnemonic::<English>::from_phrase(seed_phrase)?;

        // Convert mnemonic to seed (with optional passphrase)
        let seed = mnemonic.to_seed(""); // Empty passphrase

        let ufsk = UnifiedSpendingKey::from_seed(&TEST_NETWORK, &seed, AccountId::ZERO).unwrap();
        let unified_key = ufsk.to_unified_full_viewing_key();

        // Construct an `AccountBirthday` for the account's birthday.
        let birthday = {
            // Fetch the tree state corresponding to the last block prior to the wallet's
            // birthday height. NOTE: THIS APPROACH LEAKS THE BIRTHDAY TO THE SERVER!
            let request = service::BlockId {
                height: (birthday - 1).into(),
                ..Default::default()
            };
            let treestate = self.client.get_tree_state(request).await?.into_inner();
            AccountBirthday::from_treestate(treestate, None)
                .map_err(|_| "birthday failure")
                .expect("birthday tree step failing")
        };

        self.memory_db.import_account_ufvk(
            "default",
            &unified_key,
            &birthday,
            AccountPurpose::Spending { derivation: None },
            None,
        );

        Ok(())
    }

    pub async fn get_latest_block(&mut self) -> Result<BlockId, Box<dyn std::error::Error>> {
        let a = self
            .client
            .get_latest_block(ChainSpec::default().into_request())
            .await?;
        Ok(a.into_inner())
    }

    pub async fn wallet_summary(
        &mut self,
    ) -> Result<
        WalletSummary<<MemoryWalletDb<P> as WalletRead>::AccountId>,
        Box<dyn std::error::Error>,
    > {
        let wallet_summary = self
            .memory_db
            .get_wallet_summary(ConfirmationsPolicy::MIN)?
            .unwrap();
        Ok(wallet_summary)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let max_checkpoints = 100;
    let params = TEST_NETWORK;

    let memory_wallet = MemoryWalletDb::new(params, max_checkpoints);

    let light_client_url = "https://testnet.zec.rocks:443";

    let tls_config = ClientTlsConfig::new().with_native_roots();

    let channel = tonic::transport::Endpoint::from_static(light_client_url)
        .tls_config(tls_config)
        .unwrap()
        .connect()
        .await?;

    let mut test_client = CompactTxStreamerClient::new(channel.clone());

    let mut account = SpendingAccount::new(memory_wallet, test_client);
    let seed_phrase = "bronze foil box peace chunk use veteran course friend help chuckle ketchup destroy spin village alien embark gospel thank sustain afford hidden shadow suffer";

    let latest_block = account.get_latest_block().await?;

    println!("latest block: {:?}", latest_block);

    account.init(seed_phrase, latest_block.height).await?;
    println!("account initialized");

    Ok(())
}
