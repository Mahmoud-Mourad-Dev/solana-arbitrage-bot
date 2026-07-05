//! Yellowstone Geyser gRPC subscription (client 13.x).
//!
//! The 13.x `GeyserStream` returned by `subscribe_once` already wraps the
//! transport in `AutoReconnect` + `DedupStream`, so reconnection/backoff and
//! duplicate suppression are handled by the library — we just build the
//! account filter, open the stream, and forward account updates. (This is a
//! deliberate improvement over the TS layer's hand-rolled reconnect loop.)

use anyhow::{Context, Result};
use yellowstone_grpc_client::{GeyserGrpcClient, GeyserStream};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts,
};

/// One decoded account update from the stream.
pub struct AccountUpdate {
    pub pubkey: Vec<u8>,
    pub data: Vec<u8>,
    pub slot: u64,
}

/// Connect and open an account-filtered subscription at processed
/// commitment for the given account list.
pub async fn open_stream(
    endpoint: &str,
    x_token: Option<&str>,
    accounts: Vec<String>,
) -> Result<GeyserStream> {
    let mut builder = GeyserGrpcClient::build_from_shared(endpoint.to_string())
        .context("invalid geyser endpoint")?
        .x_token(x_token)
        .context("invalid x-token")?
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(60));

    // Enable TLS for https endpoints.
    if endpoint.starts_with("https") {
        builder = builder
            .tls_config(yellowstone_grpc_client::ClientTlsConfig::new().with_native_roots())
            .context("tls config")?;
    }

    let mut client = builder.connect().await.context("geyser connect")?;

    let mut accounts_filter = std::collections::HashMap::new();
    accounts_filter.insert(
        "pools".to_string(),
        SubscribeRequestFilterAccounts {
            account: accounts,
            owner: vec![],
            filters: vec![],
            nonempty_txn_signature: None,
            cuckoo_accounts_filter: None,
        },
    );

    let request = SubscribeRequest {
        accounts: accounts_filter,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };

    client
        .subscribe_once(request)
        .await
        .context("geyser subscribe")
}

/// Extract an account update from a raw `SubscribeUpdate`, if it is one.
pub fn extract_account_update(
    update: yellowstone_grpc_proto::geyser::SubscribeUpdate,
) -> Option<AccountUpdate> {
    match update.update_oneof? {
        UpdateOneof::Account(acc) => {
            let info = acc.account?;
            Some(AccountUpdate {
                pubkey: info.pubkey,
                data: info.data,
                slot: acc.slot,
            })
        }
        _ => None,
    }
}
