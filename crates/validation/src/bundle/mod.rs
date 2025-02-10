use std::{fmt::Debug, pin::Pin, sync::Arc};

use alloy::{
    primitives::{Address, U256},
    sol_types::{SolCall, SolValue}
};
use angstrom_metrics::validation::ValidationMetrics;
use angstrom_types::{
    contract_payloads::angstrom::{AngstromBundle, BundleGasDetails},
    primitive::TESTNET_POOL_MANAGER_ADDRESS
};
use eyre::eyre;
use futures::Future;
use pade::PadeEncode;
use revm::{
    db::CacheDB,
    inspector_handle_register,
    primitives::{keccak256, EnvWithHandlerCfg, TxKind}
};
use tokio::runtime::Handle;
use tracing::trace;

use crate::{
    common::{key_split_threadpool::KeySplitThreadpool, TokenPriceGenerator},
    order::{
        sim::console_log::CallDataInspector,
        state::db_state_utils::finders::{
            find_slot_offset_for_approval, find_slot_offset_for_balance
        }
    }
};

pub mod validator;
pub use validator::*;

pub struct BundleValidator<DB> {
    db:               CacheDB<Arc<DB>>,
    angstrom_address: Address,
    /// the address associated with this node.
    /// this will ensure the  node has access and the simulation can pass
    node_address:     Address
}

impl<DB> BundleValidator<DB>
where
    DB: Unpin + Clone + 'static + reth_provider::BlockNumReader + revm::DatabaseRef + Send + Sync,
    <DB as revm::DatabaseRef>::Error: Send + Sync + Debug
{
    pub fn new(db: Arc<DB>, angstrom_address: Address, node_address: Address) -> Self {
        Self { db: CacheDB::new(db), angstrom_address, node_address }
    }

    fn apply_slot_overrides_for_token(
        db: &mut CacheDB<Arc<DB>>,
        token: Address,
        quantity: U256,
        uniswap: Address,
        angstrom: Address
    ) where
        <DB as revm::DatabaseRef>::Error: Debug
    {
        // Find the slot for balance and approval for us to take from Uniswap
        let balance_slot = find_slot_offset_for_balance(&db, token);
        let approval_slot = find_slot_offset_for_approval(&db, token);

        // first thing we will do is setup Uniswap's token balance.
        let uniswap_balance_slot = keccak256((uniswap, balance_slot).abi_encode());
        let uniswap_approval_slot =
            keccak256((angstrom, keccak256((uniswap, approval_slot).abi_encode())).abi_encode());

        // set Uniswap's balance on the token_in
        db.insert_account_storage(token, uniswap_balance_slot.into(), U256::from(2) * quantity)
            .unwrap();
        // give angstrom approval
        db.insert_account_storage(token, uniswap_approval_slot.into(), U256::from(2) * quantity)
            .unwrap();
        // db.insert_account_storage(token_in, user_approval_slot2.into(),
        // U256::from(2) * amount_in)     .unwrap();

        // verify that everything is setup as we want
        // verify_overrides(db, token_in, token_out, amount_in, amount_out,
        // user, angstrom);
    }

    pub fn simulate_bundle(
        &self,
        sender: tokio::sync::oneshot::Sender<eyre::Result<BundleGasDetails>>,
        bundle: AngstromBundle,
        price_gen: &TokenPriceGenerator,
        thread_pool: &mut KeySplitThreadpool<
            Address,
            Pin<Box<dyn Future<Output = ()> + Send + Sync>>,
            Handle
        >,
        metrics: ValidationMetrics,
        number: u64
    ) {
        let node_address = self.node_address;
        let angstrom_address = self.angstrom_address;
        let mut db = self.db.clone();

        let conversion_lookup = price_gen.generate_lookup_map();

        thread_pool.spawn_raw(Box::pin(async move {
            let overrides = bundle.fetch_needed_overrides(number);
            for (token, slot, value) in overrides.into_slots_with_overrides(angstrom_address) {
                trace!(?token, ?slot, ?value, "Inserting bundle override");
                db.insert_account_storage(token, slot.into(), value).unwrap();
            }
            for asset in bundle.assets.iter() {
                trace!(asset = ?asset.addr, quantity = ?asset.take, uniswap_addr = ?TESTNET_POOL_MANAGER_ADDRESS, ?angstrom_address, "Inserting asset override");
                Self::apply_slot_overrides_for_token(
                    &mut db,
                    asset.addr,
                    U256::from(asset.take),
                    TESTNET_POOL_MANAGER_ADDRESS,
                    angstrom_address
                );
            }
            metrics.simulate_bundle(|| {
                let bundle = bundle.pade_encode();
                let mut console_log_inspector = CallDataInspector {};

                let mut evm = revm::Evm::builder()
                    .with_ref_db(db.clone())
                    .with_external_context(&mut console_log_inspector)
                    .with_env_with_handler_cfg(EnvWithHandlerCfg::default())
                    .append_handler_register(inspector_handle_register)
                    .modify_env(|env| {
                        env.cfg.disable_balance_check = true;
                    })
                    .modify_block_env(|env| {
                        env.number = U256::from(number + 1);
                    })
                    .modify_tx_env(|tx| {
                        tx.caller = node_address;
                        tx.transact_to = TxKind::Call(angstrom_address);
                        tx.data =
                        angstrom_types::contract_bindings::angstrom::Angstrom::executeCall::new((
                            bundle.into(),
                        ))
                        .abi_encode()
                        .into();
                    })
                    .build();

                let result = match evm
                    .transact()
                    .map_err(|e| eyre!("failed to transact with revm - {e:?}"))
                {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = sender.send(Err(eyre!(
                            "transaction simulation failed - failed to transaction with revm - \
                             {e:?}"
                        )));
                        return
                    }
                };

                if !result.result.is_success() {
                    tracing::warn!(?result.result);
                    let _ = sender.send(Err(eyre!("transaction simulation failed")));
                    return
                }

                let res = BundleGasDetails::new(conversion_lookup, result.result.gas_used());
                let _ = sender.send(Ok(res));
            });
        }))
    }
}
