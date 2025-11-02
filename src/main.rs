use std::process::CommandArgs;

use tonic::transport::{ClientTlsConfig, Channel, Endpoint};
use tonic::IntoRequest;
use zcash_client_memory::MemoryWalletDb;
use zcash_protocol::consensus::TEST_NETWORK;
use zcash_client_backend::proto::service::{ChainSpec, compact_tx_streamer_client::CompactTxStreamerClient};

mod client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let max_checkpoints = 100;
    let params = TEST_NETWORK;

    let memory_wallet = MemoryWalletDb::new(params, max_checkpoints);

    let lightwalletd_endpoint = "https://testnet.zec.rocks:443";



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

    println!("latest block: {:?}", latest_block);

    println!("Hello, world!");

    Ok(())
}
