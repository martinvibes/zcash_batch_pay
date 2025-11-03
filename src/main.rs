use std::fmt::Error;
use std::process::CommandArgs;

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::IntoRequest;
use zcash_client_backend::data_api::{
    Account, AccountBirthday, AccountPurpose, InputSource, WalletRead, WalletSummary, WalletWrite,
};
use zcash_client_backend::data_api::{WalletCommitmentTrees, Zip32Derivation};
use zcash_client_backend::keys::{UnifiedFullViewingKey, UnifiedSpendingKey};
use zcash_client_backend::proto::service;
use zcash_client_backend::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient, ChainSpec,
};
use zcash_client_memory::MemoryWalletDb;
use zcash_protocol::consensus::{Parameters, TEST_NETWORK};
use zip32::AccountId;

mod client;

pub struct SpendinAccount<P: Parameters> {
    memory_db: MemoryWalletDb<P>,
    client: CompactTxStreamerClient<Channel>,
}

impl<P: Parameters> SpendinAccount<P> {
    pub fn new(
        memory_db: MemoryWalletDb<P>,
        client: CompactTxStreamerClient<Channel>,
    ) -> SpendinAccount<P> {
        SpendinAccount { memory_db, client }
    }

    pub async fn init(
        &self,
        seed_phrase: &str,
        birthday: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let ufsk =
            UnifiedSpendingKey::from_seed(&TEST_NETWORK, seed_phrase.as_bytes(), AccountId::ZERO)
                .unwrap();
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
                .map_err(|_| Err("birthday failure"))
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

    let request = ChainSpec::default();
    let latest_block = test_client.get_latest_block(request.into_request()).await?;

    println!("latest block: {:?}", latest_block.into_inner());

    println!("Hello, world!");

    Ok(())
}
