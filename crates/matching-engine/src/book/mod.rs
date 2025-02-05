//! basic book impl so we can benchmark
use alloy_primitives::U256;
use angstrom_types::{
    matching::uniswap::PoolSnapshot,
    primitive::PoolId,
    sol_bindings::{
        grouped_orders::{GroupedVanillaOrder, OrderWithStorageData},
        Ray
    }
};
use serde::{Deserialize, Serialize};

use self::sort::SortStrategy;

pub type BookOrder = OrderWithStorageData<GroupedVanillaOrder>;

pub mod order;
pub mod sort;

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct OrderBook {
    id:   PoolId,
    amm:  Option<PoolSnapshot>,
    bids: Vec<BookOrder>,
    asks: Vec<BookOrder>
}

impl OrderBook {
    pub fn new(
        id: PoolId,
        amm: Option<PoolSnapshot>,
        mut bids: Vec<BookOrder>,
        mut asks: Vec<BookOrder>,
        sort: Option<SortStrategy>
    ) -> Self {
        // Use our sorting strategy to sort our bids and asks
        let strategy = sort.unwrap_or_default();
        strategy.sort_bids(&mut bids);
        strategy.sort_asks(&mut asks);
        Self { id, amm, bids, asks }
    }

    pub fn id(&self) -> PoolId {
        self.id
    }

    pub fn bids(&self) -> &[BookOrder] {
        &self.bids
    }

    pub fn asks(&self) -> &[BookOrder] {
        &self.asks
    }

    pub fn amm(&self) -> Option<&PoolSnapshot> {
        self.amm.as_ref()
    }

    pub fn lowest_clearing_price(&self) -> Ray {
        self.bids()
            .iter()
            .map(|bid| bid.price())
            .chain(self.asks().iter().map(|ask| ask.price()))
            .min()
            .unwrap_or_default()
    }

    pub fn highest_clearing_price(&self) -> Ray {
        self.bids()
            .iter()
            .map(|bid| bid.price())
            .chain(self.asks().iter().map(|ask| ask.price()))
            .max()
            .unwrap_or(Ray::from(U256::MAX))
    }
}

#[cfg(test)]
mod test {
    use alloy::primitives::FixedBytes;
    use angstrom_types::matching::{uniswap::LiqRange, SqrtPriceX96};

    use super::*;

    #[test]
    fn can_construct_order_book() {
        // Very basic book construction test
        let bids = vec![];
        let asks = vec![];
        let amm = PoolSnapshot::new(
            vec![LiqRange::new(90000, 110000, 10).unwrap()],
            SqrtPriceX96::at_tick(100000).unwrap()
        )
        .unwrap();
        OrderBook::new(FixedBytes::<32>::random(), Some(amm), bids, asks, None);
    }
}
