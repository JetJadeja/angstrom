use angstrom::components::initialize_strom_handles;
use angstrom_types::testnet::InitialTestnetState;
use order_pool::OrderPoolHandle;
use reth_tasks::TaskExecutor;
use telemetry::{NodeConstants, TelemetryMessage, blocklog::BlockLog};
use tokio_stream::{StreamExt, wrappers::UnboundedReceiverStream};

use crate::{
    controllers::strom::initialize_strom_components_at_block, providers::AnvilProvider,
    types::WithWalletProvider
};

pub async fn replay_stuff<Provider: WithWalletProvider>(
    provider: AnvilProvider<Provider>,
    executor: TaskExecutor,
    replay_log: BlockLog,
    initial_state: Option<InitialTestnetState>
) -> eyre::Result<()> {
    // The target block is the one in our replay
    let block_id = replay_log.blocknum();
    tracing::info!(block_id, "Starting replay");
    let telemetry_constants = if let Some(i) = initial_state {
        let log_constants = replay_log.constants().unwrap();
        NodeConstants::new(
            log_constants.node_address(),
            i.angstrom_addr,
            i.pool_manager_addr,
            log_constants.angstrom_deploy_block(),
            log_constants.gas_token_address()
        )
    } else {
        replay_log.constants().unwrap().clone()
    };

    let pools = replay_log.pool_keys().unwrap().clone();
    let strom_handles = initialize_strom_handles();
    let consensus_sender = strom_handles.consensus_tx_op.clone();

    let (pool_handle, state, canon) = initialize_strom_components_at_block(
        strom_handles,
        telemetry_constants,
        provider,
        executor,
        pools,
        block_id
    )
    .await
    .unwrap();

    let mut state_stream = UnboundedReceiverStream::new(state);

    // Playback our events in order
    for event in replay_log.events() {
        tracing::info!(?event, "Event playback");
        match event {
            TelemetryMessage::NewBlock { .. } => (),
            TelemetryMessage::NewOrder { origin, order, .. } => {
                let _res = pool_handle.new_order(*origin, order.clone()).await;
            }
            TelemetryMessage::CancelOrder { cancel, .. } => {
                let _res = pool_handle.cancel_order(cancel.clone()).await;
            }
            TelemetryMessage::Consensus { event, .. } => {
                let _res = consensus_sender.send(event.clone());
            }
            TelemetryMessage::ConsensusStateChange { state, .. } => {
                // Wait for the new state to show up as it should
                if let Some(new_state) = state_stream.next().await {
                    assert_eq!(*state, new_state, "Consensus state mismatch")
                }
            }
            TelemetryMessage::Error { message, .. } => {
                println!("Error: {message}");
            }
        }
    }
    Ok(())
}
