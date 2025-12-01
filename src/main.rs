use std::borrow::BorrowMut;
use std::convert::Infallible;
use std::fmt::Error;
use std::num::NonZeroU32;
use std::path::Path;
use std::process::CommandArgs;

use bip0039::{English, Mnemonic};
use rand::RngCore;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{IntoRequest, Response};
use zcash_address::ZcashAddress;
use zcash_address::unified::Address;
use zcash_client_backend::address;
use zcash_client_backend::data_api::wallet::{
    self, ConfirmationsPolicy, SpendingKeys, create_proposed_transactions,
    propose_standard_transfer_to_address,
};
use zcash_client_backend::data_api::{
    Account, AccountBirthday, AccountPurpose, InputSource, WalletRead, WalletSummary, WalletWrite,
};
use zcash_client_backend::data_api::{WalletCommitmentTrees, Zip32Derivation};
use zcash_client_backend::fees::StandardFeeRule;
use zcash_client_backend::keys::{UnifiedFullViewingKey, UnifiedSpendingKey};
use zcash_client_backend::proto::proposal::FeeRule;
use zcash_client_backend::proto::service::{self, BlockId, RawTransaction};
use zcash_client_backend::proto::service::{
    ChainSpec, compact_tx_streamer_client::CompactTxStreamerClient,
};
use zcash_client_backend::sync::run;
use zcash_client_backend::wallet::OvkPolicy;
use zcash_client_memory::{MemBlockCache, MemoryWalletDb};
use zcash_client_sqlite::error::SqliteClientError;
use zcash_client_sqlite::util::{Clock, SystemClock};
use zcash_client_sqlite::wallet::init::init_wallet_db;
use zcash_keys::address::Address as AddressFromZcashKeys;
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::consensus::{Parameters, TEST_NETWORK, TestNetwork};
use zcash_protocol::value::Zatoshis;
use zcash_transparent::address::TransparentAddress;
use zip32::AccountId;

use zcash_client_sqlite::{AccountUuid, FsBlockDb, ReceivedNoteId, WalletDb};

use rand_core::OsRng;
use time::OffsetDateTime;

mod client;

pub struct SpendingAccount<C: BorrowMut<rusqlite::Connection>, P: Parameters, CL: Clock, R: RngCore>
{
    db: WalletDb<C, P, CL, R>,
    client: CompactTxStreamerClient<Channel>,
    seed: Option<[u8; 64]>,
}

impl<C: BorrowMut<rusqlite::Connection>, P: Parameters, CL: Clock, R: RngCore>
    SpendingAccount<C, P, CL, R>
{
    pub fn new(
        memory_db: WalletDb<C, P, CL, R>,
        client: CompactTxStreamerClient<Channel>,
        seed_phrase: &str,
    ) -> SpendingAccount<C, P, CL, R> {
        let mnemonic = Mnemonic::<English>::from_phrase(seed_phrase).unwrap();
        // Convert mnemonic to seed (with optional passphrase)
        let seed = mnemonic.to_seed(""); // Empty passphrase 

        SpendingAccount {
            db: memory_db,
            client,
            seed: Some(seed),
        }
    }

    pub async fn init(&mut self, birthday: u64) -> Result<(), Box<dyn std::error::Error>> {
        let seed = self.seed.unwrap();

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

        self.seed = Some(seed);

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

    pub async fn get_transparent_balance(&mut self) -> Result<u64, Box<dyn std::error::Error>> {
        // Get the transparent address
        let account_ids = self.db.get_account_ids()?;
        let account_id = account_ids.get(0).ok_or("No account found")?;
        let addresses = self.db.list_addresses(account_id.clone())?;
        
        if addresses.is_empty() {
            return Ok(0);
        }
        
        let address = addresses.get(0).unwrap().address();
        let transparent_addr = match address.to_transparent_address() {
            Some(addr) => addr,
            None => return Ok(0),
        };
        
        // Query balance from lightwalletd using AddressList
        use zcash_client_backend::proto::service::AddressList;
        
        let addr_string = transparent_addr.to_zcash_address(zcash_protocol::consensus::NetworkType::Test).to_string();
        println!("Querying transparent balance for: {}", addr_string);
        
        let request = AddressList {
            addresses: vec![addr_string],
        };
        
        let response = self.client.get_taddress_balance(request).await?;
        let balance_response = response.into_inner();
        
        println!("Server response - value_zat: {}", balance_response.value_zat);
        
        let balance = balance_response.value_zat as u64;
        
        Ok(balance)
    }

    pub async fn print_address(&mut self) {
        let account_ids = self.db.get_account_ids().unwrap();
        let account_id = account_ids.get(0).unwrap();

        let addresses = self.db.list_addresses(account_id.clone()).unwrap();

        if addresses.len() != 1 {
            println!("more or less than one address found");
            return;
        }

        let address = addresses.get(0).unwrap().address();
        let unified_address = address.to_zcash_address(&TEST_NETWORK).to_string();
        
        // Extract transparent address from unified address
        let transparent_addr = address.to_transparent_address();
        
        println!("\n========================================");
        println!("YOUR WALLET ADDRESSES:");
        println!("========================================");
        println!("Unified (Shielded):");
        println!("{}", unified_address);
        println!("\nTransparent:");
        if let Some(t_addr) = transparent_addr {
            println!("{}", t_addr.to_zcash_address(zcash_protocol::consensus::NetworkType::Test).to_string());
        } else {
            println!("No transparent address available");
        }
        println!("========================================\n");
    }

    pub async fn transfer(&mut self, amount: u64, recipient: &str) {
        // Parse address (works for both shielded and transparent)
        let transparent_address_recipient = ZcashAddress::try_from_encoded(recipient)
            .unwrap()
            .convert::<AddressFromZcashKeys>()
            .unwrap();

        println!(
            "transparent address recipient: {:?}",
            transparent_address_recipient
                .to_transparent_address()
                .unwrap()
                .to_zcash_address(zcash_protocol::consensus::NetworkType::Test)
                .to_string()
        );

        let account_ids = self.db.get_account_ids().unwrap();
        let spend_from_account_id = account_ids.get(0).unwrap();

        let usks = self.db.get_unified_full_viewing_keys().unwrap();

        let usk =
            UnifiedSpendingKey::from_seed(&TEST_NETWORK, &self.seed.unwrap(), AccountId::ZERO)
                .unwrap();

        let account = self
            .db
            .get_account(spend_from_account_id.clone())
            .unwrap()
            .unwrap();

        let proposal = propose_standard_transfer_to_address::<_, _, SqliteClientError>(
            &mut self.db,
            &TEST_NETWORK,
            StandardFeeRule::Zip317,
            spend_from_account_id.clone(),
            ConfirmationsPolicy::MIN,
            &transparent_address_recipient,
            Zatoshis::const_from_u64(amount),
            None,
            None,
            zcash_protocol::ShieldedProtocol::Orchard,
        )
        .unwrap();

        // Build and broadcast transaction
        let prover = LocalTxProver::bundled();
        let spending_keys = SpendingKeys::from_unified_spending_key(usk.clone());

        let txids = create_proposed_transactions::<
            _,
            _,
            <WalletDb<C, P, CL, R> as InputSource>::Error,
            _,
            Infallible,
            _,
        >(
            &mut self.db,
            &TEST_NETWORK,
            &prover,
            &prover,
            &spending_keys,
            OvkPolicy::Sender,
            &proposal,
        )
        .unwrap();

        let txid = txids.first().clone();

        let transaction = self
            .db
            .get_transaction(txid)
            .unwrap()
            .ok_or("Transaction not found")
            .unwrap();

        let mut tx_bytes = Vec::new();
        transaction.write(&mut tx_bytes).unwrap();

        let response = self
            .client
            .send_transaction(RawTransaction {
                data: tx_bytes,
                height: 0,
            })
            .await
            .unwrap();

        if response.get_ref().error_code != 0 {
            println!("Network error: {}", response.get_ref().error_message);
            return;
        }

        println!("transaction sent {:?}", txid.to_string());
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load environment variables from .env file
    dotenvy::dotenv().ok();

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

    let seed_phrase = std::env::var("SEED_PHRASE").expect("SEED_PHRASE must be set in .env file");
    let mut account = SpendingAccount::new(wallet_db, test_client, &seed_phrase);

    let latest_block = account.get_latest_block().await?;
    println!("latest block: {:?}", latest_block);

    // Check if account already exists, only initialize if it doesn't
    let account_ids = account.db.get_account_ids().unwrap();
    if account_ids.is_empty() {
        // Use your actual wallet birthday height
        let wallet_birthday = 3670000;
        account.init(wallet_birthday).await?;
        println!("account initialized with birthday: {}", wallet_birthday);
    } else {
        println!("account already exists, skipping initialization");
    }

    account.sync().await;

    let wallet_summary = account.wallet_summary().await?;
    
    // Get transparent balance from lightwalletd server
    let transparent_balance_zatoshis = account.get_transparent_balance().await?;
    
    // Display balance in a readable format
    println!("\n========================================");
    println!("WALLET BALANCE:");
    println!("========================================");
    
    for (account_id, balance) in wallet_summary.account_balances() {
        let sapling_zec = balance.sapling_balance().spendable_value().into_u64() as f64 / 100_000_000.0;
        let orchard_zec = balance.orchard_balance().spendable_value().into_u64() as f64 / 100_000_000.0;
        let transparent_zec = transparent_balance_zatoshis as f64 / 100_000_000.0;
        let total_zec = sapling_zec + orchard_zec + transparent_zec;
        
        println!("Account: {:?}", account_id);
        println!("  Sapling:     {:.8} ZEC", sapling_zec);
        println!("  Orchard:     {:.8} ZEC", orchard_zec);
        println!("  Transparent: {:.8} ZEC (from server)", transparent_zec);
        println!("  ----------------------------");
        println!("  TOTAL:       {:.8} ZEC", total_zec);
    }
    
    println!("========================================\n");

    account.print_address().await;

    // Uncomment below to send a transaction
    // let to_address = "tmC3F49jzP8ocdkGkgSgTrMLCZPRD6sKpDK";
    // account.transfer(100, to_address).await;

    Ok(())
}
