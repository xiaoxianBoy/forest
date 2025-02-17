// Copyright 2019-2024 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

mod auth_api;
mod auth_layer;
mod beacon_api;
mod chain_api;
mod channel;
mod common_api;
mod eth_api;
mod gas_api;
mod mpool_api;
mod net_api;
mod node_api;
mod state_api;
mod sync_api;
mod wallet_api;

pub use error::JsonRpcError;
use reflect::Ctx;
pub use reflect::RpcMethodExt;
mod error;
mod reflect;

use std::net::SocketAddr;
use std::sync::Arc;

use crate::key_management::KeyStore;
use crate::rpc::auth_layer::AuthLayer;
use crate::rpc::channel::RpcModule as FilRpcModule;
pub use crate::rpc::channel::CANCEL_METHOD_NAME;
use crate::rpc::{
    beacon_api::beacon_get_entry,
    common_api::{session, shutdown, start_time, version},
    state_api::*,
};
use crate::rpc_api::{
    auth_api::*, beacon_api::*, chain_api::*, common_api::*, eth_api::*, gas_api::*, mpool_api::*,
    net_api::*, node_api::NODE_STATUS, state_api::*, sync_api::*, wallet_api::*,
};

use fvm_ipld_blockstore::Blockstore;
use hyper::server::conn::AddrStream;
use hyper::service::{make_service_fn, service_fn};
use jsonrpsee::{
    core::RegisterMethodError,
    server::{stop_channel, RpcModule, RpcServiceBuilder, Server, StopHandle, TowerServiceBuilder},
    Methods,
};
use tokio::sync::mpsc::Sender;
use tokio::sync::RwLock;
use tower::Service;
use tracing::info;

use self::chain_api::ChainGetPath;
use self::reflect::openrpc_types::ParamStructure;

const MAX_RESPONSE_BODY_SIZE: u32 = 16 * 1024 * 1024;

/// This is where you store persistent data, or at least access to stateful
/// data.
pub struct RPCState<DB> {
    pub keystore: Arc<RwLock<KeyStore>>,
    pub chain_store: Arc<crate::chain::ChainStore<DB>>,
    pub state_manager: Arc<crate::state_manager::StateManager<DB>>,
    pub mpool: Arc<crate::message_pool::MessagePool<crate::message_pool::MpoolRpcProvider<DB>>>,
    pub bad_blocks: Arc<crate::chain_sync::BadBlockCache>,
    pub sync_state: Arc<parking_lot::RwLock<crate::chain_sync::SyncState>>,
    pub network_send: flume::Sender<crate::libp2p::NetworkMessage>,
    pub network_name: String,
    pub start_time: chrono::DateTime<chrono::Utc>,
    pub beacon: Arc<crate::beacon::BeaconSchedule>,
}

#[derive(Clone)]
struct PerConnection<RpcMiddleware, HttpMiddleware> {
    methods: Methods,
    stop_handle: StopHandle,
    svc_builder: TowerServiceBuilder<RpcMiddleware, HttpMiddleware>,
    keystore: Arc<RwLock<KeyStore>>,
}

pub async fn start_rpc<DB>(
    state: RPCState<DB>,
    rpc_endpoint: SocketAddr,
    forest_version: &'static str,
    shutdown_send: Sender<()>,
) -> anyhow::Result<()>
where
    DB: Blockstore + Send + Sync + 'static,
{
    // `Arc` is needed because we will share the state between two modules
    let state = Arc::new(state);
    let keystore = state.keystore.clone();
    let (mut module, _schema) = create_module(state.clone());

    // TODO(forest): https://github.com/ChainSafe/forest/issues/4032
    #[allow(deprecated)]
    register_methods(
        &mut module,
        u64::from(state.state_manager.chain_config().block_delay_secs),
        forest_version,
        shutdown_send,
    )?;

    let mut pubsub_module = FilRpcModule::default();

    pubsub_module.register_channel("Filecoin.ChainNotify", {
        let state_clone = state.clone();
        move |params| chain_api::chain_notify(params, &state_clone)
    })?;
    module.merge(pubsub_module)?;

    let (stop_handle, _handle) = stop_channel();

    let per_conn = PerConnection {
        methods: module.into(),
        stop_handle: stop_handle.clone(),
        svc_builder: Server::builder()
            // Default size (10 MiB) is not enough for methods like `Filecoin.StateMinerActiveSectors`
            .max_response_body_size(MAX_RESPONSE_BODY_SIZE)
            .to_service_builder(),
        keystore,
    };

    let make_service = make_service_fn(move |_conn: &AddrStream| {
        let per_conn = per_conn.clone();

        async move {
            anyhow::Ok(service_fn(move |req| {
                let PerConnection {
                    methods,
                    stop_handle,
                    svc_builder,
                    keystore,
                } = per_conn.clone();

                let headers = req.headers().clone();
                let rpc_middleware = RpcServiceBuilder::new().layer(AuthLayer {
                    headers,
                    keystore: keystore.clone(),
                });

                let mut svc = svc_builder
                    .set_rpc_middleware(rpc_middleware)
                    .build(methods, stop_handle);

                async move { svc.call(req).await }
            }))
        }
    });

    info!("Ready for RPC connections");
    hyper::Server::bind(&rpc_endpoint)
        .serve(make_service)
        .await?;

    info!("Stopped accepting RPC connections");

    Ok(())
}

fn create_module<DB>(
    state: Arc<RPCState<DB>>,
) -> (
    RpcModule<Arc<RPCState<DB>>>,
    reflect::openrpc_types::OpenRPC,
)
where
    DB: Blockstore + Send + Sync + 'static,
{
    let mut module = reflect::SelfDescribingRpcModule::new(state, ParamStructure::ByPosition);
    ChainGetPath::register(&mut module);
    module.finish()
}

#[deprecated = "methods should use `create_module`"]
fn register_methods<DB>(
    module: &mut RpcModule<Arc<RPCState<DB>>>,
    block_delay: u64,
    forest_version: &'static str,
    shutdown_send: Sender<()>,
) -> Result<(), RegisterMethodError>
where
    DB: Blockstore + Send + Sync + 'static,
{
    use auth_api::*;
    use chain_api::*;
    use eth_api::*;
    use gas_api::*;
    use mpool_api::*;
    use net_api::*;
    use node_api::*;
    use sync_api::*;
    use wallet_api::*;

    // Auth API
    module.register_async_method(AUTH_NEW, auth_new::<DB>)?;
    module.register_async_method(AUTH_VERIFY, auth_verify::<DB>)?;
    // Beacon API
    module.register_async_method(BEACON_GET_ENTRY, beacon_get_entry::<DB>)?;
    // Chain API
    module.register_async_method(CHAIN_GET_MESSAGE, chain_get_message::<DB>)?;
    module.register_async_method(CHAIN_EXPORT, chain_export::<DB>)?;
    module.register_async_method(CHAIN_READ_OBJ, chain_read_obj::<DB>)?;
    module.register_async_method(CHAIN_HAS_OBJ, chain_has_obj::<DB>)?;
    module.register_async_method(CHAIN_GET_BLOCK_MESSAGES, chain_get_block_messages::<DB>)?;
    module.register_async_method(CHAIN_GET_TIPSET_BY_HEIGHT, chain_get_tipset_by_height::<DB>)?;
    module.register_async_method(
        CHAIN_GET_TIPSET_AFTER_HEIGHT,
        chain_get_tipset_after_height::<DB>,
    )?;
    module.register_async_method(CHAIN_GET_GENESIS, |_, state| chain_get_genesis::<DB>(state))?;
    module.register_async_method(CHAIN_GET_TIPSET, chain_get_tipset::<DB>)?;
    module.register_async_method(CHAIN_HEAD, |_, state| chain_head::<DB>(state))?;
    module.register_async_method(CHAIN_GET_BLOCK, chain_get_block::<DB>)?;
    module.register_async_method(CHAIN_SET_HEAD, chain_set_head::<DB>)?;
    module.register_async_method(CHAIN_GET_MIN_BASE_FEE, chain_get_min_base_fee::<DB>)?;
    module.register_async_method(
        CHAIN_GET_MESSAGES_IN_TIPSET,
        chain_get_messages_in_tipset::<DB>,
    )?;
    module.register_async_method(CHAIN_GET_PARENT_MESSAGES, chain_get_parent_messages::<DB>)?;
    module.register_async_method(CHAIN_GET_PARENT_RECEIPTS, chain_get_parent_receipts::<DB>)?;
    // Message Pool API
    module.register_async_method(MPOOL_GET_NONCE, mpool_get_nonce::<DB>)?;
    module.register_async_method(MPOOL_PENDING, mpool_pending::<DB>)?;
    module.register_async_method(MPOOL_PUSH, mpool_push::<DB>)?;
    module.register_async_method(MPOOL_PUSH_MESSAGE, mpool_push_message::<DB>)?;
    // Sync API
    module.register_async_method(SYNC_CHECK_BAD, sync_check_bad::<DB>)?;
    module.register_async_method(SYNC_MARK_BAD, sync_mark_bad::<DB>)?;
    module.register_async_method(SYNC_STATE, |_, state| sync_state::<DB>(state))?;
    // Wallet API
    module.register_async_method(WALLET_BALANCE, wallet_balance::<DB>)?;
    module.register_async_method(WALLET_DEFAULT_ADDRESS, wallet_default_address::<DB>)?;
    module.register_async_method(WALLET_EXPORT, wallet_export::<DB>)?;
    module.register_async_method(WALLET_HAS, wallet_has::<DB>)?;
    module.register_async_method(WALLET_IMPORT, wallet_import::<DB>)?;
    module.register_async_method(WALLET_LIST, wallet_list::<DB>)?;
    module.register_async_method(WALLET_NEW, wallet_new::<DB>)?;
    module.register_async_method(WALLET_SET_DEFAULT, wallet_set_default::<DB>)?;
    module.register_async_method(WALLET_SIGN, wallet_sign::<DB>)?;
    module.register_async_method(WALLET_VALIDATE_ADDRESS, |params, _| {
        wallet_validate_address(params)
    })?;
    module.register_async_method(WALLET_VERIFY, |params, _| wallet_verify(params))?;
    module.register_async_method(WALLET_DELETE, wallet_delete::<DB>)?;
    // State API
    module.register_async_method(STATE_CALL, state_call::<DB>)?;
    module.register_async_method(STATE_REPLAY, state_replay::<DB>)?;
    module.register_async_method(STATE_NETWORK_NAME, |_, state| {
        state_network_name::<DB>(state)
    })?;
    module.register_async_method(STATE_NETWORK_VERSION, state_get_network_version::<DB>)?;
    module.register_async_method(STATE_ACCOUNT_KEY, state_account_key::<DB>)?;
    module.register_async_method(STATE_LOOKUP_ID, state_lookup_id::<DB>)?;
    module.register_async_method(STATE_GET_ACTOR, state_get_actor::<DB>)?;
    module.register_async_method(STATE_MARKET_BALANCE, state_market_balance::<DB>)?;
    module.register_async_method(STATE_MARKET_DEALS, state_market_deals::<DB>)?;
    module.register_async_method(STATE_MINER_INFO, state_miner_info::<DB>)?;
    module.register_async_method(MINER_GET_BASE_INFO, miner_get_base_info::<DB>)?;
    module.register_async_method(STATE_MINER_ACTIVE_SECTORS, state_miner_active_sectors::<DB>)?;
    module.register_async_method(STATE_MINER_SECTOR_COUNT, state_miner_sector_count::<DB>)?;
    module.register_async_method(STATE_MINER_FAULTS, state_miner_faults::<DB>)?;
    module.register_async_method(STATE_MINER_RECOVERIES, state_miner_recoveries::<DB>)?;
    module.register_async_method(
        STATE_MINER_AVAILABLE_BALANCE,
        state_miner_available_balance::<DB>,
    )?;
    module.register_async_method(STATE_MINER_POWER, state_miner_power::<DB>)?;
    module.register_async_method(STATE_MINER_DEADLINES, state_miner_deadlines::<DB>)?;
    module.register_async_method(STATE_LIST_MESSAGES, state_list_messages::<DB>)?;
    module.register_async_method(STATE_LIST_MINERS, state_list_miners::<DB>)?;
    module.register_async_method(
        STATE_MINER_PROVING_DEADLINE,
        state_miner_proving_deadline::<DB>,
    )?;
    module.register_async_method(STATE_GET_RECEIPT, state_get_receipt::<DB>)?;
    module.register_async_method(STATE_WAIT_MSG, state_wait_msg::<DB>)?;
    module.register_async_method(STATE_SEARCH_MSG, state_search_msg::<DB>)?;
    module.register_async_method(STATE_SEARCH_MSG_LIMITED, state_search_msg_limited::<DB>)?;
    module.register_async_method(STATE_FETCH_ROOT, state_fetch_root::<DB>)?;
    module.register_async_method(
        STATE_GET_RANDOMNESS_FROM_TICKETS,
        state_get_randomness_from_tickets::<DB>,
    )?;
    module.register_async_method(
        STATE_GET_RANDOMNESS_FROM_BEACON,
        state_get_randomness_from_beacon::<DB>,
    )?;
    module.register_async_method(STATE_READ_STATE, state_read_state::<DB>)?;
    module.register_async_method(STATE_CIRCULATING_SUPPLY, state_circulating_supply::<DB>)?;
    module.register_async_method(STATE_SECTOR_GET_INFO, state_sector_get_info::<DB>)?;
    module.register_async_method(
        STATE_VERIFIED_CLIENT_STATUS,
        state_verified_client_status::<DB>,
    )?;
    module.register_async_method(
        STATE_VM_CIRCULATING_SUPPLY_INTERNAL,
        state_vm_circulating_supply_internal::<DB>,
    )?;
    module.register_async_method(STATE_MARKET_STORAGE_DEAL, state_market_storage_deal::<DB>)?;
    module.register_async_method(MSIG_GET_AVAILABLE_BALANCE, msig_get_available_balance::<DB>)?;
    module.register_async_method(MSIG_GET_PENDING, msig_get_pending::<DB>)?;
    // Gas API
    module.register_async_method(GAS_ESTIMATE_FEE_CAP, gas_estimate_fee_cap::<DB>)?;
    module.register_async_method(GAS_ESTIMATE_GAS_LIMIT, gas_estimate_gas_limit::<DB>)?;
    module.register_async_method(GAS_ESTIMATE_GAS_PREMIUM, gas_estimate_gas_premium::<DB>)?;
    module.register_async_method(GAS_ESTIMATE_MESSAGE_GAS, gas_estimate_message_gas::<DB>)?;
    // Common API
    module.register_method(VERSION, move |_, _| version(block_delay, forest_version))?;
    module.register_method(SESSION, |_, _| session())?;
    module.register_async_method(SHUTDOWN, move |_, _| shutdown(shutdown_send.clone()))?;
    module.register_method(START_TIME, move |_, state| start_time::<DB>(state))?;
    // Net API
    module.register_async_method(NET_ADDRS_LISTEN, |_, state| net_addrs_listen::<DB>(state))?;
    module.register_async_method(NET_PEERS, |_, state| net_peers::<DB>(state))?;
    module.register_async_method(NET_LISTENING, |_, _| net_listening())?;
    module.register_async_method(NET_INFO, |_, state| net_info::<DB>(state))?;
    module.register_async_method(NET_CONNECT, net_connect::<DB>)?;
    module.register_async_method(NET_DISCONNECT, net_disconnect::<DB>)?;
    module.register_async_method(NET_AGENT_VERSION, net_agent_version::<DB>)?;
    module.register_async_method(NET_AUTO_NAT_STATUS, net_auto_nat_status::<DB>)?;
    module.register_async_method(NET_VERSION, net_version::<DB>)?;
    // Node API
    module.register_async_method(NODE_STATUS, |_, state| node_status::<DB>(state))?;
    // Eth API
    module.register_async_method(ETH_ACCOUNTS, |_, _| eth_accounts())?;
    module.register_async_method(ETH_BLOCK_NUMBER, |_, state| eth_block_number::<DB>(state))?;
    module.register_async_method(ETH_CHAIN_ID, |_, state| eth_chain_id::<DB>(state))?;
    module.register_async_method(ETH_GAS_PRICE, |_, state| eth_gas_price::<DB>(state))?;
    module.register_async_method(ETH_GET_BALANCE, eth_get_balance::<DB>)?;
    module.register_async_method(ETH_SYNCING, eth_syncing::<DB>)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::task::JoinSet;

    use crate::{
        blocks::Chain4U,
        chain::ChainStore,
        chain_sync::SyncConfig,
        db::car::PlainCar,
        genesis::get_network_name_from_genesis,
        message_pool::{MessagePool, MpoolRpcProvider},
        networks::ChainConfig,
        state_manager::StateManager,
        KeyStoreConfig,
    };

    use super::*;

    // TODO(forest): https://github.com/ChainSafe/forest/issues/4047
    //               `tokio` shouldn't be necessary
    #[tokio::test]
    async fn openrpc() {
        let (_, spec) = create_module(Arc::new(RPCState::calibnet()));
        insta::assert_yaml_snapshot!(spec);
    }

    impl RPCState<Chain4U<PlainCar<&'static [u8]>>> {
        pub fn calibnet() -> Self {
            let chain_store = Arc::new(ChainStore::calibnet());
            let genesis = chain_store.genesis_block_header();
            let state_manager = Arc::new(
                StateManager::new(
                    chain_store.clone(),
                    Arc::new(ChainConfig::calibnet()),
                    Arc::new(SyncConfig::default()),
                )
                .unwrap(),
            );
            let beacon = Arc::new(
                state_manager
                    .chain_config()
                    .get_beacon_schedule(genesis.timestamp),
            );
            let (network_send, _) = flume::bounded(0);
            let network_name = get_network_name_from_genesis(genesis, &state_manager).unwrap();
            let message_pool = MessagePool::new(
                MpoolRpcProvider::new(chain_store.publisher().clone(), state_manager.clone()),
                network_name.clone(),
                network_send.clone(),
                Default::default(),
                state_manager.chain_config().clone(),
                &mut JoinSet::default(),
            )
            .unwrap();
            RPCState {
                state_manager,
                keystore: Arc::new(RwLock::new(KeyStore::new(KeyStoreConfig::Memory).unwrap())),
                mpool: Arc::new(message_pool),
                bad_blocks: Default::default(),
                sync_state: Default::default(),
                network_send,
                network_name,
                start_time: Default::default(),
                chain_store,
                beacon,
            }
        }
    }
}
