use std::borrow::BorrowMut;
use std::fmt::Error;
use std::num::NonZeroU32;
use std::path::Path;
use std::process::CommandArgs;

use bip0039::{English, Mnemonic};
use rand::RngCore;
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
use zcash_client_backend::sync::run;
use zcash_client_memory::{MemBlockCache, MemoryWalletDb};
use zcash_client_sqlite::util::{Clock, SystemClock};
use zcash_client_sqlite::wallet::init::init_wallet_db;
use zcash_protocol::consensus::{Parameters, TEST_NETWORK};
use zip32::AccountId;

use zcash_client_sqlite::{AccountUuid, FsBlockDb, WalletDb};

use rand_core::OsRng;
use time::OffsetDateTime;

mod client;

pub struct SpendingAccount<C: BorrowMut<rusqlite::Connection>, P: Parameters, CL: Clock, R: RngCore>
{
    db: WalletDb<C, P, CL, R>,
    client: CompactTxStreamerClient<Channel>,
}

impl<C: BorrowMut<rusqlite::Connection>, P: Parameters, CL: Clock, R: RngCore>
    SpendingAccount<C, P, CL, R>
{
    pub fn new(
        memory_db: WalletDb<C, P, CL, R>,
        client: CompactTxStreamerClient<Channel>,
    ) -> SpendingAccount<C, P, CL, R> {
        SpendingAccount {
            db: memory_db,
            client,
        }
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

        self.db
            .import_account_ufvk(
                "default",
                &unified_key,
                &birthday,
                AccountPurpose::Spending { derivation: None },
                None,
            )
            .unwrap();

        Ok(())
    }

    pub async fn sync(&mut self) {
        println!("syncing...");

        // TODO: This should be held in the Wallet struct so we can download in parallel
        let db_cache = MemBlockCache::new();

        run(
            &mut self.client,
            &TEST_NETWORK,
            &db_cache,
            &mut self.db,
            100,
        )
        .await
        .unwrap();

        println!("synced");
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
        WalletSummary<<WalletDb<C, P, CL, R> as WalletRead>::AccountId>,
        Box<dyn std::error::Error>,
    > {
        let wallet_summary = self
            .db
            .get_wallet_summary(ConfirmationsPolicy::MIN)?
            .unwrap();
        Ok(wallet_summary)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let max_checkpoints = 100;
    let params = TEST_NETWORK;

    // 3. Use SQLite wallet database (fully implemented!)
    let db_data_path = Path::new("./wallet.db");
    let mut wallet_db = WalletDb::for_path(db_data_path, params, SystemClock, OsRng)?;

    // 4. Initialize the wallet if it's new
    // (This creates the database schema)
    init_wallet_db(&mut wallet_db, None).unwrap();

    let memory_wallet = MemoryWalletDb::new(params, max_checkpoints);

    let light_client_url = "https://testnet.zec.rocks:443";

    let tls_config = ClientTlsConfig::new().with_native_roots();

    let channel = tonic::transport::Endpoint::from_static(light_client_url)
        .tls_config(tls_config)
        .unwrap()
        .connect()
        .await?;

    let mut test_client = CompactTxStreamerClient::new(channel.clone());

    let mut account = SpendingAccount::new(wallet_db, test_client);
    let seed_phrase = "bronze foil box peace chunk use veteran course friend help chuckle ketchup destroy spin village alien embark gospel thank sustain afford hidden shadow suffer";

    let latest_block = account.get_latest_block().await?;

    println!("latest block: {:?}", latest_block);

    // account.init(seed_phrase, latest_block.height).await?;
    println!("account initialized");

    account.sync().await;

    let wallet_summary = account.wallet_summary().await?;

    Ok(())
}
