use std::pin::Pin;

use alloy_primitives::{FixedBytes, U256};
use angstrom_eth::manager::ChainExt;
use angstrom_types::{
    orders::{OrderId, OrderOrigin, OrderPriorityData},
    sol_bindings::{
        grouped_orders::{AllOrders, OrderWithStorageData},
        RawPoolOrder
    },
    testnet::InitialTestnetState
};
use futures::{Future, StreamExt};
use matching_engine::{
    book::{sort::SortStrategy, BookOrder, OrderBook},
    matcher::binary_search::BinarySearchMatcher,
    strategy::{MatchingStrategy, SimpleCheckpointStrategy}
};
use reth_provider::{noop::NoopProvider, CanonStateSubscriptions};
use reth_tasks::TaskExecutor;
use testing_tools::{
    agents::AgentConfig,
    controllers::enviroments::AngstromTestnet,
    order_generator::{self, GeneratedPoolOrders, OrderGenerator},
    types::{
        actions::WithAction, checked_actions::WithCheckedAction, checks::WithCheck,
        config::DevnetConfig
    }
};
use tracing::{debug, info, span, Instrument, Level};

use crate::cli::compare_engines::CompareEnginesCli;

pub async fn compare_matching_engines(
    executor: TaskExecutor,
    cli: CompareEnginesCli
) -> eyre::Result<()> {
    let config = cli.testnet_config.make_config()?;

    let agents = vec![cmp_agent];
    tracing::info!("spinning up e2e nodes for angstrom");

    // spawn testnet
    let testnet = AngstromTestnet::spawn_testnet(NoopProvider::default(), config, agents).await?;
    tracing::info!("e2e testnet is alive");

    executor
        .spawn_critical_blocking("testnet", testnet.run_to_completion(executor.clone()))
        .await?;
    Ok(())
}

fn cmp_agent<'a>(
    _: &'a InitialTestnetState,
    agent_config: AgentConfig
) -> Pin<Box<dyn Future<Output = eyre::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let mut generator = OrderGenerator::new(
            agent_config.uniswap_pools.clone(),
            agent_config.current_block,
            2..5,
            0.1..0.6
        );

        let mut stream =
            agent_config
                .state_provider
                .canonical_state_stream()
                .map(|node| match node {
                    reth_provider::CanonStateNotification::Commit { new }
                    | reth_provider::CanonStateNotification::Reorg { new, .. } => new.tip_number()
                });

        tokio::spawn(
            async move {
                tracing::info!("waiting for new block");

                while let Some(block) = stream.next().await {
                    generator.new_block(block);
                    let new_orders = generator.generate_orders();
                    for orders in new_orders {
                        let GeneratedPoolOrders { pool_id, tob, book } = orders;
                        let pool = agent_config.uniswap_pools.get(&pool_id).unwrap();
                        let (_, _, amm) = pool.read().unwrap().fetch_pool_snapshot().unwrap();
                        let asks =
                            book.iter()
                                .filter(|f| !f.is_bid())
                                .map(|ask| {
                                    OrderWithStorageData {
                invalidates: vec![],
                order: ask.clone(),
                priority_data: OrderPriorityData {
                    price:     *ask.price(),
                    volume:    ask.amount(),
                    gas:       U256::ZERO,
                    gas_units: 0
                },
                is_bid: false,
                is_valid: true,
                is_currently_valid: true,
                order_id: OrderId {
                    flash_block:     None,
                    reuse_avoidance: angstrom_types::sol_bindings::RespendAvoidanceMethod::Block(0),
                    hash:            Default::default(),
                    address:         Default::default(),
                    deadline:        None,
                    pool_id,
                    location:        angstrom_types::orders::OrderLocation::Limit
                },
                pool_id,
                valid_block: 0,
                tob_reward: U256::ZERO
                            }
                                })
                                .collect::<Vec<BookOrder>>();
                        let bids =
                            book.iter()
                                .filter(|f| f.is_bid())
                                .map(|bid| {
                                    OrderWithStorageData {
                invalidates: vec![],
                order: bid.clone(),
                priority_data: OrderPriorityData {
                    price:     *bid.price(),
                    volume:    bid.amount(),
                    gas:       U256::ZERO,
                    gas_units: 0
                },
                is_bid: true,
                is_valid: true,
                is_currently_valid: true,
                order_id: OrderId {
                    flash_block:     None,
                    reuse_avoidance: angstrom_types::sol_bindings::RespendAvoidanceMethod::Block(0),
                    hash:            Default::default(),
                    address:         Default::default(),
                    deadline:        None,
                    pool_id,
                    location:        angstrom_types::orders::OrderLocation::Limit
                },
                pool_id,
                valid_block: 0,
                tob_reward: U256::ZERO
                            }
                                })
                                .collect::<Vec<BookOrder>>();

                        let book = OrderBook::new(
                            pool_id,
                            Some(amm),
                            bids,
                            asks,
                            Some(SortStrategy::ByPriceByVolume)
                        );

                        let tob = OrderWithStorageData {
                            invalidates: vec![],
                            order: tob.clone(),
                            priority_data: OrderPriorityData {
                                price:     tob.limit_price(),
                                volume:    tob.amount(),
                                gas:       U256::ZERO,
                                gas_units: 0
                            },
                            is_bid: tob.is_bid(),
                            is_valid: true,
                            is_currently_valid: true,
                            order_id: OrderId {
                                flash_block: None,
                                reuse_avoidance:
                                    angstrom_types::sol_bindings::RespendAvoidanceMethod::Block(0),
                                hash: Default::default(),
                                address: Default::default(),
                                deadline: None,
                                pool_id,
                                location: angstrom_types::orders::OrderLocation::Limit
                            },
                            pool_id,
                            valid_block: 0,
                            tob_reward: U256::ZERO
                        };
                        let bisection = BinarySearchMatcher::new(&book).solution(Some(tob.clone()));

                        let debt_engine = SimpleCheckpointStrategy::run(&book)
                            .unwrap()
                            .solution(Some(tob));

                        println!(
                            "\n\n\n\n bisection_results{:#?}\n\n debt_results:{:#?}",
                            bisection, debt_engine
                        );
                    }
                }
            }
            .instrument(span!(Level::ERROR, "matching engine cmp", ?agent_config.agent_id))
        );
        Ok(())
    })
}
