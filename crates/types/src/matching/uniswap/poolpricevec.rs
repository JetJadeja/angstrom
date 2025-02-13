use std::{cmp::Ordering, collections::HashMap, ops::Neg};

use alloy::primitives::{Uint, I256, U256};
use eyre::{eyre, Context};
use uniswap_v3_math::{
    sqrt_price_math::{
        _get_amount_0_delta, _get_amount_1_delta, get_next_sqrt_price_from_input,
        get_next_sqrt_price_from_output
    },
    swap_math::compute_swap_step
};

use super::{poolprice::PoolPrice, Direction, LiqRangeRef, Quantity, Tick};
use crate::{
    matching::{math::low_to_high, Ray, SqrtPriceX96},
    orders::OrderPrice
};

#[derive(Clone, Debug)]
pub struct SwapStep<'a> {
    start_price: SqrtPriceX96,
    end_price:   SqrtPriceX96,
    d_t0:        u128,
    d_t1:        u128,
    liq_range:   LiqRangeRef<'a>
}

impl<'a> SwapStep<'a> {
    /// Create a SwapStep that covers the portion of the liquidity range
    /// contained within the bounds of the two prices given.  If the
    /// liquidity range is completely contained within the interval `[start,
    /// end)` then the generated SwapStep will cover the entire liquidity
    /// range.
    pub fn for_range(
        start: &PoolPrice<'a>,
        end: &PoolPrice<'a>,
        liq_range: &LiqRangeRef<'a>
    ) -> eyre::Result<Self> {
        Self::for_price_range(&start.price, &end.price, liq_range)
    }

    /// Create a SwapStep that covers the portion of the liquidity range
    /// contained within the bounds of the two prices given.  If the
    /// liquidity range is completely contained within the interval `[start,
    /// end)` then the generated SwapStep will cover the entire liquidity
    /// range.
    ///
    /// This method uses raw prices instead of PoolPrice references
    pub fn for_price_range(
        start: &SqrtPriceX96,
        end: &SqrtPriceX96,
        liq_range: &LiqRangeRef<'a>
    ) -> eyre::Result<Self> {
        // Sort our incoming prices into the low and high price
        let (low, high) = low_to_high(start, end);
        let low_tick = low.to_tick()?;
        let high_tick = high.to_tick()?;

        // Make sure both of our price ticks are within bounds, otherwise return an
        // error
        if low_tick >= liq_range.upper_tick || high_tick < liq_range.lower_tick {
            return Err(eyre!("Ticks out of bounds, unable to construct step"))
        }

        let bounded_low = if low_tick >= liq_range.lower_tick {
            *low
        } else {
            SqrtPriceX96::at_tick(liq_range.lower_tick)?
        };

        let bounded_high = if high_tick < liq_range.upper_tick {
            *high
        } else {
            SqrtPriceX96::at_tick(liq_range.upper_tick)?
        };

        if start > end {
            // If the price is decreasing go from high price to low price
            Self::compute_info(bounded_high, bounded_low, *liq_range)
        } else {
            // If the price is increasing go from low price to high price
            Self::compute_info(bounded_low, bounded_high, *liq_range)
        }
    }

    /// Creates a SwapStep that goes from the price given to the edge of the
    /// liquidity range that the price is associated with in the given Direction
    pub fn to_bound(start: PoolPrice<'a>, direction: Direction) -> eyre::Result<Self> {
        let end = start.liq_range.clone().end_price(direction);
        Self::from_prices(start, end)
    }

    /// Creates a SwapStep that covers the entirety of the provided liquidity
    /// range
    pub fn whole_range(range: LiqRangeRef<'a>, direction: Direction) -> eyre::Result<Self> {
        let start = range.start_price(direction);
        let end = range.end_price(direction);
        Self::from_prices(start, end)
    }

    /// Creates a SwapStep that covers the range between two prices, provided
    /// those prices are both within the same liquidity range
    pub fn from_prices(start: PoolPrice<'a>, end: PoolPrice<'a>) -> eyre::Result<Self> {
        if start.liq_range != end.liq_range {
            return Err(eyre!(
                "A SwapStep can only cover one liquidity range, provided prices are from \
                 different ranges"
            ));
        }
        Self::compute_info(start.price, end.price, start.liq_range)
    }

    /// Internal method for computing swap step details
    fn compute_info(
        start_price: SqrtPriceX96,
        end_price: SqrtPriceX96,
        liq_range: LiqRangeRef<'a>
    ) -> eyre::Result<Self> {
        // Make sure our prices are in the appropriate range.
        let (low_price, high_price) = low_to_high(&start_price, &end_price);
        // Low price is valid if it's within our liquidity range
        let low_price_valid = liq_range.price_in_range(*low_price);
        // High price is valid if it's either within our liquidity range or at the very
        // top of the liquidity range
        let high_price_valid = liq_range.price_in_range(*high_price)
            || *high_price == SqrtPriceX96::at_tick(liq_range.upper_tick).unwrap();
        if !(low_price_valid && high_price_valid) {
            return Err(eyre!("Price outside liquidity range"))
        }

        let liquidity = liq_range.liquidity;
        let (round_0, round_1) = match Direction::from_prices(start_price, end_price) {
            Direction::BuyingT0 => (false, true),
            Direction::SellingT0 => (true, false)
        };
        let sqrt_ratio_a_x_96 = start_price.into();
        let sqrt_ratio_b_x_96 = end_price.into();
        let d_t0 = _get_amount_0_delta(sqrt_ratio_a_x_96, sqrt_ratio_b_x_96, liquidity, round_0)
            .unwrap_or(Uint::from(0))
            .to();
        let d_t1 = _get_amount_1_delta(sqrt_ratio_a_x_96, sqrt_ratio_b_x_96, liquidity, round_1)
            .unwrap_or(Uint::from(0))
            .to();
        Ok(Self { start_price, end_price, d_t0, d_t1, liq_range })
    }

    pub fn start_price(&self) -> SqrtPriceX96 {
        self.start_price
    }

    pub fn end_price(&self) -> SqrtPriceX96 {
        self.end_price
    }

    /// Find the average settling price for this step, if the step is "empty"
    /// (no t0 or t1 was moved) we'll return None
    pub fn avg_price(&self) -> Option<Ray> {
        if self.empty() {
            None
        } else {
            Some(Ray::calc_price(U256::from(self.d_t0), U256::from(self.d_t1)))
        }
    }

    pub fn liquidity(&self) -> u128 {
        self.liq_range.liquidity
    }

    pub fn input(&self) -> u128 {
        if self.end_price > self.start_price {
            self.d_t1
        } else {
            self.d_t0
        }
    }

    pub fn output(&self) -> u128 {
        if self.end_price > self.start_price {
            self.d_t0
        } else {
            self.d_t1
        }
    }

    /// An empty step has no motion in t0 or t1, but is sometimes present to
    /// make sure we record a price that started precisely on a tick boundary
    pub fn empty(&self) -> bool {
        self.d_t0 == 0 || self.d_t1 == 0
    }
}

#[derive(Debug)]
pub struct DonationResult {
    /// HashMap from liquidity range bounds `(lower_tick, upper_tick)` to
    /// donation quantity in T0
    pub tick_donations: HashMap<(Tick, Tick), u128>,
    pub final_price:    SqrtPriceX96,
    /// Total amount of donation in T0
    pub total_donated:  u128,
    /// Total amount of "tribute" taken by Angstrom in T0
    pub tribute:        u128
}

#[derive(Clone, Debug)]
pub struct PoolPriceVec<'a> {
    pub start_bound: PoolPrice<'a>,
    pub end_bound:   PoolPrice<'a>,
    pub d_t0:        u128,
    pub d_t1:        u128,
    pub steps:       Option<Vec<SwapStep<'a>>>
}

impl<'a> PoolPriceVec<'a> {
    /// Create a new PoolPriceVec from a start and end price with minimal
    /// checks.  The liquidity used will be the liquidity available at the
    /// `start_bound`.  This should be used in quick-and-dirty situations
    /// when we know that we're building a short-range PoolPriceVec that exists
    /// within a single liquidity position
    pub fn new(start_bound: PoolPrice<'a>, end_bound: PoolPrice<'a>) -> Self {
        let (d_t0, d_t1) =
            Self::delta_to_price(start_bound.price, end_bound.price, start_bound.liquidity());
        Self { start_bound, end_bound, d_t0, d_t1, steps: None }
    }

    pub fn input(&self) -> u128 {
        if self.end_bound.price > self.start_bound.price {
            self.d_t1
        } else {
            self.d_t0
        }
    }

    pub fn output(&self) -> u128 {
        if self.end_bound.price > self.start_bound.price {
            self.d_t0
        } else {
            self.d_t1
        }
    }

    pub fn steps(&self) -> Option<&Vec<SwapStep>> {
        self.steps.as_ref()
    }

    /// Create a new PoolPriceVec from a start and end price with full safety
    /// checks and with the ability to span liquidity boundaries.
    pub fn from_price_range(start: PoolPrice<'a>, end: PoolPrice<'a>) -> eyre::Result<Self> {
        // If the two prices aren't from the same pool, we should error
        if !std::ptr::eq(start.liq_range.pool_snap, end.liq_range.pool_snap) {
            return Err(eyre!("Cannot create a price range from prices not in the same pool"));
        }

        let steps: Vec<SwapStep> = start
            .liq_range
            .pool_snap
            .ranges_for_ticks(start.tick, end.tick)?
            .iter()
            .map(|liq_range| SwapStep::for_range(&start, &end, liq_range))
            .collect::<eyre::Result<Vec<SwapStep>>>()?;

        Self::from_steps(start, end, steps)
    }

    pub fn to_price_bound(start: PoolPrice<'a>, end: SqrtPriceX96) -> eyre::Result<Self> {
        let end_tick = end.to_tick()?;
        let steps = start
            .liq_range
            .pool_snap
            .ranges_for_ticks(start.tick, end_tick)?
            .iter()
            .map(|liq_range| {
                let (start, end) = low_to_high(&start.price, &end);
                SwapStep::for_price_range(start, end, liq_range)
            })
            .collect::<eyre::Result<Vec<SwapStep>>>()?;
        let end = if let Some(l) = steps.last() {
            let tick = l.end_price.to_tick()?;
            PoolPrice { liq_range: l.liq_range, price: l.end_price, tick }
        } else {
            return Err(eyre!("Unable to find actual end price"))
        };
        Self::from_steps(start, end, steps)
    }

    pub fn to_upper(start: PoolPrice<'a>) -> eyre::Result<Self> {
        let end = if let Some(range) = start
            .liq_range
            .pool_snap
            .get_range_for_tick(start.liq_range.upper_tick)
        {
            range.start_price(Direction::BuyingT0)
        } else {
            start.liq_range.end_price(Direction::BuyingT0)
        };
        let steps = vec![SwapStep::to_bound(start.clone(), Direction::BuyingT0)?];
        Self::from_steps(start, end, steps)
    }

    fn from_steps(
        start: PoolPrice<'a>,
        end: PoolPrice<'a>,
        steps: Vec<SwapStep<'a>>
    ) -> eyre::Result<Self> {
        let (d_t0, d_t1) = steps.iter().fold((0_u128, 0_u128), |(t0, t1), step| {
            (t0.saturating_add(step.d_t0), t1.saturating_add(step.d_t1))
        });
        Ok(Self { start_bound: start, end_bound: end, d_t0, d_t1, steps: Some(steps) })
    }

    pub fn from_swap(
        start: PoolPrice<'a>,
        direction: Direction,
        quantity: Quantity
    ) -> eyre::Result<Self> {
        let fee_pips = 0;
        let mut total_in = U256::ZERO;
        let mut total_out = U256::ZERO;
        let mut current_price = start.price;
        let mut current_liq_range: Option<_> = Some(start.liquidity_range());
        let mut left_to_swap = quantity.magnitude();

        let mut steps: Vec<SwapStep> = Vec::new();

        let is_swap_input = direction.is_input(&quantity);

        while left_to_swap > 0 {
            // Update our current liquidiy range
            let liq_range =
                current_liq_range.ok_or_else(|| eyre!("Unable to find next liquidity range"))?;
            // Compute our swap towards the appropriate end of our current liquidity bound
            let target_tick = liq_range.end_tick(direction);
            let target_price = SqrtPriceX96::at_tick(target_tick)?;
            // If our target price is equal to our current price, we're precisely at the
            // "bottom" of a liquidity range and we can skip this computation as
            // it will be a null step - but we're going to add the null step anyways for
            // donation purposes
            if target_price == current_price {
                steps.push(SwapStep {
                    start_price: current_price,
                    end_price: target_price,
                    d_t0: 0,
                    d_t1: 0,
                    liq_range
                });
                current_liq_range = liq_range.next(direction);
                continue;
            }

            let amount_remaining = if is_swap_input {
                // Exact in is calculated with a positive quantity
                I256::unchecked_from(left_to_swap)
            } else {
                // Exact out is calculated with a negative quantity
                I256::unchecked_from(left_to_swap).neg()
            };

            // Now we can compute our step
            let (fin_price, amount_in, amount_out, amount_fee) = compute_swap_step(
                current_price.into(),
                target_price.into(),
                liq_range.liquidity(),
                amount_remaining,
                fee_pips
            )
            .wrap_err_with(|| {
                format!(
                    "Unable to compute swap step from tick {:?} to {}",
                    current_price.to_tick(),
                    target_tick
                )
            })?;

            // See how much output we have yet to go
            if is_swap_input {
                // If our left_to_swap is the input, we want to subtract the amount in that was
                // allocated and the fee
                left_to_swap = left_to_swap.saturating_sub(amount_in.saturating_to());
                left_to_swap = left_to_swap.saturating_sub(amount_fee.saturating_to());
            } else {
                // If our left_to_swap is the output, we want to subtract the amount out that
                // was allocated
                left_to_swap = left_to_swap.saturating_sub(amount_out.saturating_to());
            }

            // Add the amount in and fee to our cost
            total_in += amount_in;
            total_in += amount_fee;
            // Add the amount out to our output
            total_out += amount_out;

            // Based on our direction, sort out what our token0 and token1 are
            let (d_t0, d_t1) = direction.sort_tokens(amount_in.to(), amount_out.to());

            // Push this step onto our list of swap steps
            steps.push(SwapStep {
                start_price: current_price,
                end_price: SqrtPriceX96::from(fin_price),
                d_t0,
                d_t1,
                liq_range
            });
            // (avg_price, end_price, amount_out, liq_range));

            // If we're going to be continuing, move on to the next liquidity range
            current_liq_range = liq_range.next(direction);
            current_price = SqrtPriceX96::from(fin_price);
        }

        let (d_t0, d_t1) = direction.sort_tokens(total_in.to(), total_out.to());
        let end_bound = start.liq_range.pool_snap.at_price(current_price)?;
        Ok(Self { start_bound: start, end_bound, d_t0, d_t1, steps: Some(steps) })
    }

    /// Builds a DonationResult based on the goal of making sure that the net
    /// price for all sections of this swap is as close to the final price as
    /// possible.  All donations are T0.
    pub fn t0_donation_to_end_price(&self, total_donation: u128) -> DonationResult {
        tracing::trace!(total_donation, "Performing donation to end price");
        // If we have no steps we can just short-circuit this whole thing and take the
        // whole donation as tribute
        let Some(steps) = self.steps.as_ref() else {
            return DonationResult {
                tick_donations: HashMap::new(),
                final_price:    self.end_bound.price,
                total_donated:  0,
                tribute:        total_donation
            };
        };

        let mut remaining_donation = total_donation;
        let price_dropping = self.start_bound.price > self.end_bound.price;
        // The price will drop if we are adding T0 to the pool to get T1 out.  In these
        // cases we should round up.  If the price is increasing, we're atting T1 to the
        // pool to get T0 out and we should round down
        let round_up = price_dropping;

        let mut current_blob: Option<(u128, u128)> = None;

        let steps_iter = steps.iter().filter(|s| !s.empty());

        for step in steps_iter {
            // If our current blob is empty, we can just insert the current step's stats
            // into it
            let Some((c_t0, c_t1)) = &mut current_blob else {
                current_blob = Some((step.d_t0, step.d_t1));
                continue;
            };

            // Find the average price of our current step and get our existing blob to
            // that price
            let target_price = step.avg_price().unwrap();
            let target_t0 = target_price.inverse_quantity(*c_t1, round_up);
            // The step cost is the difference between the amount of t0 we actually moved
            // and the amount we should have moved to be at this step's average price
            let step_cost = c_t0.abs_diff(target_t0);

            // If the move costs as much or less than what we have to spend, we've completed
            // this step and can merge blobs
            tracing::info!(?remaining_donation, ?step_cost);
            let step_complete = remaining_donation >= step_cost && remaining_donation != 0;

            let increment = std::cmp::min(remaining_donation, step_cost);
            if price_dropping {
                // If the price T1/T0 is dropping, we're going to be giving our LPs MORE T0 in
                // exchange for the T1 they pay us
                *c_t0 += increment;
            } else {
                // If the price T1/T0 is increasing, we're going to be refunding T0 to the LPs,
                // meaning they have effectively given us LESS T0 for the T1 we paid them
                *c_t0 = c_t0.saturating_sub(increment)
            }
            remaining_donation -= increment;

            if step_complete {
                // If we had enough reward to complete this step, we continue and merge this
                // step into the blob
                *c_t0 += step.d_t0;
                *c_t1 += step.d_t1;
            } else {
                // If we didn't have enough reward to complete this step, we're done
                break;
            }
        }

        // At this point, all of our swap is within the blob.  If we have additional
        // donation, we want to distribute it ALL to the blob to get to the best price
        // possible.
        if let Some((c_t0, _)) = current_blob.as_mut() {
            if price_dropping {
                *c_t0 += remaining_donation
            } else {
                *c_t0 = c_t0.saturating_sub(remaining_donation)
            }
        }
        // Now we can find our filled price - if the price is dropping we want to round
        // down otherwise we want to round up.  Note that this diverges from other
        // rounding being done.
        let filled_price =
            current_blob.map(|(t0, t1)| Ray::calc_price_generic(t0, t1, !price_dropping));

        tracing::trace!(?filled_price, swap_end_price = ?self.end_bound.price, "Found post-donation price");
        // We've now found our filled price, we can allocate our reward to each tick
        // based on how much it costs to bring them to that price.
        // We can start remaining_donation over
        remaining_donation = total_donation;
        let mut total_donated = 0_u128;
        let tick_donations: HashMap<(Tick, Tick), u128> = steps
            .iter()
            .map(|step| {
                let reward = if let Some(f) = filled_price {
                    // T1 is constant, so we need to know how much t0 we need
                    let target_t0 = f.inverse_quantity(step.d_t1, round_up);
                    if price_dropping {
                        // If the filled_price should be lower than our current price, then our
                        // target T0 is MORE than we have in this step
                        std::cmp::min(remaining_donation, target_t0.saturating_sub(step.d_t0))
                    } else {
                        // If the filled_price should be higher than our current price, then our
                        // target T0 is LESS than we have in this step
                        std::cmp::min(remaining_donation, step.d_t0.saturating_sub(target_t0))
                    }
                } else {
                    0
                };
                remaining_donation -= reward;
                total_donated += reward;
                // We always donate to the lower tick of our liquidity range as that is the
                // appropriate initialized tick to target
                ((step.liq_range.lower_tick(), step.liq_range.upper_tick()), reward)
            })
            .collect();
        let tribute = total_donation.saturating_sub(total_donated);

        DonationResult {
            tick_donations,
            final_price: self.end_bound.as_sqrtpricex96(),
            total_donated,
            tribute
        }
    }

    // Seems to be unused
    pub fn to_price(&self, target: SqrtPriceX96) -> Option<Self> {
        let (start_in_bounds, end_in_bounds) = if self.is_buy() {
            (Ordering::Greater, Ordering::Less)
        } else {
            (Ordering::Less, Ordering::Greater)
        };
        if self.start_bound.price.cmp(&target) == start_in_bounds {
            if self.end_bound.price.cmp(&target) == end_in_bounds {
                // If the target price is between the start and end bounds, make a subvec
                let new_upper = self.start_bound.liq_range.pool_snap.at_price(target).ok()?;
                Some(Self::new(self.start_bound.clone(), new_upper))
            } else {
                // If the target price is beyond the end bound in the appropriate direction,
                // return a copy of this existing vector
                Some(self.clone())
            }
        } else {
            // If the target price is equal to or beyond the start price in an inappropriate
            // direction, there is no vector to be made
            None
        }
    }

    /// A very raw delta to a specific price presuming the liquidity is constant
    /// for the duration of the swap
    fn delta_to_price(
        start_price: SqrtPriceX96,
        end_price: SqrtPriceX96,
        liquidity: u128
    ) -> (u128, u128) {
        let sqrt_ratio_a_x_96 = start_price.into();
        let sqrt_ratio_b_x_96 = end_price.into();
        let d_t0 = _get_amount_0_delta(sqrt_ratio_a_x_96, sqrt_ratio_b_x_96, liquidity, false)
            .unwrap_or(Uint::from(0));
        let d_t1 = _get_amount_1_delta(sqrt_ratio_a_x_96, sqrt_ratio_b_x_96, liquidity, false)
            .unwrap_or(Uint::from(0));
        (d_t0.to(), d_t1.to())
    }

    pub fn is_buy(&self) -> bool {
        self.start_bound.price < self.end_bound.price
    }

    /// Returns `(d_t0, d_t1, price)`
    pub fn quantity(&self, target_price: OrderPrice) -> (u128, u128, OrderPrice) {
        let t: SqrtPriceX96 = Ray::from(*target_price).into();

        // If our target price is past our end bound, our quantity is the entire range
        if (self.is_buy() && t > self.end_bound.price) || t < self.end_bound.price {
            return (self.d_t0, self.d_t1, OrderPrice::from(self.end_bound.price));
        }

        let (d_t0, d_t1) =
            Self::delta_to_price(self.start_bound.price, t, self.start_bound.liquidity());
        (d_t0, d_t1, target_price)
    }

    // Maybe it's OK that I don't check the price again here because in the matching
    // algo I've only offered a quantity bounded by the price, so we should
    // always be OK?
    pub fn fill(&self, quantity: u128) -> Self {
        let liquidity = self.start_bound.liquidity();
        let end_sqrt_price = if self.is_buy() {
            get_next_sqrt_price_from_output(
                self.start_bound.price.into(),
                liquidity,
                U256::from(quantity),
                true
            )
            .map(SqrtPriceX96::from)
            .unwrap()
        } else {
            get_next_sqrt_price_from_input(
                self.start_bound.price.into(),
                liquidity,
                U256::from(quantity),
                true
            )
            .map(SqrtPriceX96::from)
            .unwrap()
        };
        let (d_t0, d_t1) = Self::delta_to_price(self.start_bound.price, end_sqrt_price, liquidity);
        let mut end_bound = self.start_bound.clone();
        end_bound.price = end_sqrt_price;
        Self { end_bound, start_bound: self.start_bound.clone(), d_t0, d_t1, steps: None }
    }

    pub fn swap_to_price(
        start: PoolPrice<'a>,
        final_price: SqrtPriceX96,
        direction: Direction
    ) -> eyre::Result<Self> {
        let fee_pips = 0;
        let mut total_in = U256::ZERO;
        let mut total_out = U256::ZERO;
        let mut current_price = start.price;

        let mut current_liq_range: Option<_> = Some(start.liquidity_range());
        let mut left_to_swap = u128::MAX;

        let mut steps: Vec<SwapStep> = Vec::new();
        // if we are a bid, then we want exact out, else we are a ask and want exact in.
        let is_swap_input = if direction.is_bid() { false } else { true };

        while left_to_swap > 0 && final_price != current_price {
            // Update our current liquidiy range
            let liq_range =
                current_liq_range.ok_or_else(|| eyre!("Unable to find next liquidity range"))?;
            // Compute our swap towards the appropriate end of our current liquidity bound
            let target_tick = liq_range.end_tick(direction);
            let target_price = if direction.is_bid() {
                // if we are a bid, means that start_price < end_price.
                // as we are swapping up, we want to take the min
                SqrtPriceX96::at_tick(target_tick)?.min(final_price)
            } else {
                // if we are zfo, means that start_price > end and we are going down.
                // we always want to choose max
                SqrtPriceX96::at_tick(target_tick)?.max(final_price)
            };

            // If our target price is equal to our current price, we're precisely at the
            // "bottom" of a liquidity range and we can skip this computation as
            // it will be a null step - but we're going to add the null step anyways for
            // donation purposes
            if target_price == current_price {
                steps.push(SwapStep {
                    start_price: current_price,
                    end_price: target_price,
                    d_t0: 0,
                    d_t1: 0,
                    liq_range
                });
                current_liq_range = liq_range.next(direction);
                continue;
            }

            let amount_remaining = if is_swap_input {
                // Exact in is calculated with a positive quantity
                I256::unchecked_from(left_to_swap)
            } else {
                // Exact out is calculated with a negative quantity
                I256::unchecked_from(left_to_swap).neg()
            };

            // Now we can compute our step
            let (fin_price, amount_in, amount_out, amount_fee) = compute_swap_step(
                current_price.into(),
                target_price.into(),
                liq_range.liquidity(),
                amount_remaining,
                fee_pips
            )
            .wrap_err_with(|| {
                format!(
                    "Unable to compute swap step from tick {:?} to {}",
                    current_price.to_tick(),
                    target_tick
                )
            })?;

            // See how much output we have yet to go
            if is_swap_input {
                // If our left_to_swap is the input, we want to subtract the amount in that was
                // allocated and the fee
                left_to_swap = left_to_swap.saturating_sub(amount_in.saturating_to());
                left_to_swap = left_to_swap.saturating_sub(amount_fee.saturating_to());
            } else {
                // If our left_to_swap is the output, we want to subtract the amount out that
                // was allocated
                left_to_swap = left_to_swap.saturating_sub(amount_out.saturating_to());
            }

            // Add the amount in and fee to our cost
            total_in += amount_in;
            total_in += amount_fee;
            // Add the amount out to our output
            total_out += amount_out;

            // Based on our direction, sort out what our token0 and token1 are
            let (d_t0, d_t1) = direction.sort_tokens(amount_in.to(), amount_out.to());

            // Push this step onto our list of swap steps
            steps.push(SwapStep {
                start_price: current_price,
                end_price: SqrtPriceX96::from(fin_price),
                d_t0,
                d_t1,
                liq_range
            });
            // (avg_price, end_price, amount_out, liq_range));

            // If we're going to be continuing, move on to the next liquidity range
            current_liq_range = liq_range.next(direction);
            current_price = SqrtPriceX96::from(fin_price);
        }

        let (d_t0, d_t1) = direction.sort_tokens(total_in.to(), total_out.to());
        let end_bound = start.liq_range.pool_snap.at_price(current_price)?;
        Ok(Self { start_bound: start, end_bound, d_t0, d_t1, steps: Some(steps) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::uniswap::{LiqRange, PoolSnapshot};

    #[test]
    fn can_construct_pricevec() {
        let liquidity = 1_000_000_000_000_000_u128;
        let pool = PoolSnapshot::new(
            vec![LiqRange { liquidity, lower_tick: 100000, upper_tick: 100100 }],
            SqrtPriceX96::at_tick(100050).unwrap()
        )
        .unwrap();
        PoolPriceVec::new(pool.current_price(), pool.current_price());
    }

    #[test]
    fn will_span_segments() {
        let seg_1_liq = 1_000_000_000_000_000_u128;
        let seg_2_liq = 1_000_000_000_000_u128;
        let segment_1 = LiqRange { liquidity: seg_1_liq, lower_tick: 100000, upper_tick: 100050 };
        let segment_2 = LiqRange { liquidity: seg_2_liq, lower_tick: 100050, upper_tick: 100100 };
        let cur_price = SqrtPriceX96::at_tick(100025).unwrap();
        let end_price = SqrtPriceX96::at_tick(100075).unwrap();
        let pool = PoolSnapshot::new(vec![segment_1, segment_2], cur_price).unwrap();
        let start_bound = pool.current_price();
        let end_bound = pool.at_price(end_price).unwrap();
        let pricevec = PoolPriceVec::from_price_range(start_bound, end_bound).unwrap();
        assert!(pricevec.steps.is_some(), "No steps in our price vector");
        let steps = pricevec.steps.as_ref().unwrap();
        assert_eq!(steps.len(), 2, "Wrong number of steps in our price vector");

        // Make sure the total swap is the same
        let mid_price = SqrtPriceX96::at_tick(100050).unwrap();
        let amount_remaining = I256::unchecked_from(pricevec.d_t1);
        let (next_price, first_step_in, first_step_out, _) =
            compute_swap_step(cur_price.into(), mid_price.into(), seg_1_liq, amount_remaining, 0)
                .unwrap();
        assert_eq!(
            first_step_in,
            U256::from(steps[0].d_t1),
            "Token1 payment incorrect for first step"
        );
        assert_eq!(
            first_step_out,
            U256::from(steps[0].d_t0),
            "Token0 received incorrect for first step"
        );
        assert_eq!(SqrtPriceX96::from(next_price), mid_price, "Price after first step mismatched");
        let (post_second_price, second_step_in, second_step_out, _) =
            compute_swap_step(mid_price.into(), end_price.into(), seg_2_liq, amount_remaining, 0)
                .unwrap();
        assert_eq!(
            second_step_in,
            U256::from(steps[1].d_t1),
            "Token1 payment incorrect for second step"
        );
        assert_eq!(
            second_step_out,
            U256::from(steps[1].d_t0),
            "Token0 received incorrect for second step"
        );
        assert_eq!(
            SqrtPriceX96::from(post_second_price),
            end_price,
            "Price after second step mismatched"
        );

        assert_eq!(
            first_step_in + second_step_in,
            U256::from(pricevec.d_t1),
            "Final vec doesn't match input sum"
        );
        assert_eq!(
            first_step_out + second_step_out,
            U256::from(pricevec.d_t0),
            "Final vec doesn't match input sum"
        );
    }

    #[test]
    fn swaps_in_both_directions() {
        let seg_1_liq = 1_000_000_000_000_000_u128;
        let seg_2_liq = 1_000_000_000_000_u128;
        let segment_1 = LiqRange { liquidity: seg_1_liq, lower_tick: 100000, upper_tick: 100050 };
        let segment_2 = LiqRange { liquidity: seg_2_liq, lower_tick: 100050, upper_tick: 100100 };
        let segment_3 = LiqRange { liquidity: seg_1_liq, lower_tick: 100100, upper_tick: 100150 };
        let segment_4 = LiqRange { liquidity: seg_2_liq, lower_tick: 100150, upper_tick: 100200 };
        let cur_price = SqrtPriceX96::at_tick(100150).unwrap();
        let end_price = SqrtPriceX96::at_tick(100050).unwrap();
        let pool =
            PoolSnapshot::new(vec![segment_1, segment_2, segment_3, segment_4], cur_price).unwrap();
        let low_start = pool.at_price(end_price).unwrap();
        let high_start = pool.current_price();

        let buy_vec = PoolPriceVec::from_swap(
            low_start,
            Direction::BuyingT0,
            Quantity::Token0(1234567890_u128)
        )
        .expect("Failed to generate pricevec from swap");

        assert!(buy_vec.d_t0 > 0, "No t0 moved in buy vec");
        assert!(buy_vec.d_t1 > 0, "No t1 moved in buy vec");

        let sell_vec = PoolPriceVec::from_swap(
            high_start,
            Direction::SellingT0,
            Quantity::Token0(1234567890_u128)
        )
        .expect("Failed to generate pricevec from swap");

        assert!(sell_vec.d_t0 > 0, "No t0 moved in buy vec");
        assert!(sell_vec.d_t1 > 0, "No t1 moved in buy vec");
    }

    #[test]
    fn will_include_all_steps() {
        let seg_1_liq = 1_000_000_000_000_000_u128;
        let seg_2_liq = 1_000_000_000_000_u128;
        let segment_1 = LiqRange { liquidity: seg_1_liq, lower_tick: 100000, upper_tick: 100050 };
        let segment_2 = LiqRange { liquidity: seg_2_liq, lower_tick: 100050, upper_tick: 100100 };
        let segment_3 = LiqRange { liquidity: seg_1_liq, lower_tick: 100100, upper_tick: 100150 };
        let segment_4 = LiqRange { liquidity: seg_2_liq, lower_tick: 100150, upper_tick: 100200 };
        let cur_price = SqrtPriceX96::at_tick(100150).unwrap();
        let end_price = SqrtPriceX96::at_tick(100050).unwrap();
        let pool =
            PoolSnapshot::new(vec![segment_1, segment_2, segment_3, segment_4], cur_price).unwrap();
        let start_bound = pool.current_price();
        let end_bound = pool.at_price(end_price).unwrap();
        let pricevec = PoolPriceVec::from_price_range(start_bound.clone(), end_bound).unwrap();
        let steps = pricevec.steps.expect("Steps not generated");
        assert_eq!(steps.len(), 3, "Incorrect number of steps generated");

        let from_swap_vec = PoolPriceVec::from_swap(
            start_bound,
            Direction::SellingT0,
            Quantity::Token0(pricevec.d_t0)
        )
        .expect("Failed to generate pricevec from swap");
        let from_swap_steps = from_swap_vec.steps.expect("Steps not generated");
        assert_eq!(from_swap_steps.len(), 3, "Incorrect number of steps generated");
        assert_eq!(
            from_swap_vec.end_bound.price, pricevec.end_bound.price,
            "Final prices not equal"
        );
    }
}
