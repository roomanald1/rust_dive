use core::cmp::min;

/// Number of 5-minute intervals in a standard 6.5-hour trading day (390 mins / 5)
pub const NUM_BINS: usize = 78;
pub const BIN_DURATION_SECS: f64 = 300.0; // 5 minutes
const INV_BIN_DURATION_SECS: f64 = 1.0 / BIN_DURATION_SECS; // Fast multiply inverse
/// Static configuration for the VWAP Scheduler
pub struct VwapConfig {
    pub parent_qty: u64,
    /// Bin weights normalized such that sum(weights) == 1.0 (Fixed-size array)
    pub historical_weights: [f64; NUM_BINS],
    /// Maximum participation cap per bin (e.g., 0.15 for 15%)
    pub max_participation_rate: f64,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct VwapState {
    pub current_bin: usize,
    pub executed_qty: u64,
    pub remaining_qty: u64,
}

pub struct VwapScheduler {
    config: VwapConfig,
    state: VwapState,
    // Pre-computed once at setup:
    // suffix_weights[0]  = weight[0] + weight[1] + ... + weight[77] (always 1.0)
    // suffix_weights[1]  = weight[1] + weight[2] + ... + weight[77]
    // ...
    // suffix_weights[77] = weight[77]
    suffix_weights: [f64; NUM_BINS],
}

impl VwapScheduler {
    pub fn new(config: VwapConfig) -> Self {
        let parent_qty = config.parent_qty;
        let mut suffix_weights = [0.0; NUM_BINS];

        // Pre-compute suffix sums in reverse: suffix_weights[k] = sum(weights[k..N])
        let mut accum = 0.0;
        let mut i = NUM_BINS;
        while i > 0 {
            i -= 1;
            accum += config.historical_weights[i];
            suffix_weights[i] = accum;
        }

        Self {
            config,
            state: VwapState {
                current_bin: 0,
                executed_qty: 0,
                remaining_qty: parent_qty,
            },
            suffix_weights,
        }
    }

    /// live_bin_volume - the amount of volume in this bin period (5mins)
    /// In reality this might be called multiple times in a 5 min period on each trade (tick by tick).
    /// In order to calculate the live bin volume you would adjust the live_bin_volume by the
    /// elapsed fraction of the current bin (e.g., elapsed_time / total_bin_time) to extrapolate
    /// expected full-bin volume, or pass the running trade volume accumulated since the start of
    /// the current bin window.
    /// Example: If 3,000 trade 1.5 mins into a 5-min bin (30% elapsed),
    /// extrapolated live_bin_volume = 3,000 / 0.30 = 10,000.
    #[inline(always)]
    pub fn calculate_child_order(&mut self, live_bin_volume: u64, expected_bin_volume: u64) -> u64 {
        let bin = self.state.current_bin;

        //avoid bounds checks with unsafe
        let weight_k = unsafe { *self.config.historical_weights.get_unchecked(bin) };
        let rem_weight = unsafe { *self.suffix_weights.get_unchecked(bin) };

        let base_slice = if rem_weight > 0.0 {
            (self.state.remaining_qty as f64) * (weight_k / rem_weight)
        } else {
            self.state.remaining_qty as f64
        };

        let volume_ratio = if expected_bin_volume > 0 {
            (live_bin_volume as f64) / (expected_bin_volume as f64)
        } else {
            1.0
        };

        let target_unclamped = base_slice * volume_ratio;

        // Prevent market impact during noise/spikes
        let max_bin_qty = (live_bin_volume as f64 * self.config.max_participation_rate) as u64;

        // Bound final child size
        let target_qty = (target_unclamped + 0.5) as u64;
        let clamped_qty = min(target_qty, max_bin_qty);
        let final_child_qty = min(clamped_qty, self.state.remaining_qty);

        final_child_qty
    }

    #[inline(always)]
    pub fn on_fill(&mut self, filled_qty: u64) {
        let actual_fill = min(filled_qty, self.state.remaining_qty);
        self.state.executed_qty += actual_fill;
        self.state.remaining_qty -= actual_fill;
    }

    #[inline(always)]
    pub fn advance_bin(&mut self) {
        if self.state.current_bin < NUM_BINS {
            self.state.current_bin += 1;
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum OrderAction {
    NewOrder { qty: u64 },
    ModifyOrder { new_qty: u64 },
    CancelOrder,
}

pub struct OrderManager {
    scheduler: VwapScheduler,
    expected_bin_volumes: [u64; NUM_BINS],

    // Live tracking state per bin
    raw_bin_volume: u64,
    filled_in_current_bin: u64,
    working_order_qty: u64,

    // Controls exchange order pacing (prevents spamming ticks)
    min_trade_threshold: u64,
}

impl OrderManager {
    pub fn new(
        config: VwapConfig,
        expected_bin_volumes: [u64; NUM_BINS],
        min_trade_threshold: u64,
    ) -> Self {
        Self {
            scheduler: VwapScheduler::new(config),
            expected_bin_volumes,
            raw_bin_volume: 0,
            filled_in_current_bin: 0,
            working_order_qty: 0,
            min_trade_threshold,
        }
    }

    /// Primary Tick Ingestion Handler
    /// Returns an optional action to send to the exchange execution gateway
    pub fn on_market_trade(
        &mut self,
        trade_qty: u64,
        elapsed_bin_secs: f64,
    ) -> Option<OrderAction> {
        let current_bin = self.scheduler.state.current_bin;
        if current_bin >= NUM_BINS {
            return None;
        }

        // 1. Accumulate raw volume for the current bin
        self.raw_bin_volume += trade_qty;

        // 2. Extrapolate intra-bin volume to 5-minute equivalent
        // Guard against div-by-zero during the first fraction of a second
        let elapsed_fraction = (elapsed_bin_secs * INV_BIN_DURATION_SECS).clamp(0.01, 1.0);
        let extrapolated_live_vol = (self.raw_bin_volume as f64 / elapsed_fraction) as u64;

        // 3. Compute target slice for full 5-minute bin
        let expected_vol = self.expected_bin_volumes[current_bin];
        let target_bin_qty = self
            .scheduler
            .calculate_child_order(extrapolated_live_vol, expected_vol);

        // 4. Calculate net remaining target for THIS bin (subtracting what already filled)
        let net_target_qty = target_bin_qty.saturating_sub(self.filled_in_current_bin);

        // 5. Check if size drift crosses the execution threshold
        let drift = net_target_qty.abs_diff(self.working_order_qty);

        if drift >= self.min_trade_threshold {
            if self.working_order_qty == 0 && net_target_qty > 0 {
                self.working_order_qty = net_target_qty;
                Some(OrderAction::NewOrder {
                    qty: net_target_qty,
                })
            } else if net_target_qty == 0 && self.working_order_qty > 0 {
                self.working_order_qty = 0;
                Some(OrderAction::CancelOrder)
            } else {
                self.working_order_qty = net_target_qty;
                Some(OrderAction::ModifyOrder {
                    new_qty: net_target_qty,
                })
            }
        } else {
            None // No exchange action required (drift within threshold tolerance)
        }
    }

    /// Asynchronous Execution Fill Handler
    pub fn on_execution_fill(&mut self, filled_qty: u64) {
        self.scheduler.on_fill(filled_qty);
        self.filled_in_current_bin += filled_qty;
        self.working_order_qty = self.working_order_qty.saturating_sub(filled_qty);
    }

    /// Called at 5-minute interval boundaries (09:35, 09:40, etc.)
    pub fn on_bin_transition(&mut self) {
        self.scheduler.advance_bin();
        self.raw_bin_volume = 0;
        self.filled_in_current_bin = 0;
        self.working_order_qty = 0;
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn mock_setup() -> OrderManager {
        let weight = 1.0 / (NUM_BINS as f64);
        let config = VwapConfig {
            parent_qty: 78_000,
            historical_weights: [weight; NUM_BINS],
            max_participation_rate: 0.50,
        };
        let expected_volumes = [10_000; NUM_BINS]; // Expect 10,000 bin
        let min_threshold = 100; // Require 100 drift to trigger exchange action

        OrderManager::new(config, expected_volumes, min_threshold)
    }

    #[test]
    fn test_tick_extrapolation_and_threshold_pacing() {
        let mut om = mock_setup();

        // 1. First trade tick at 1.5 mins (30% elapsed). 3,000 traded -> On target pace!
        // Extrapolated volume = 3,000 / 0.30 = 10,000. Ratio = 1.0. Target = 1,000 .
        let action = om.on_market_trade(3_000, 90.0);
        assert_eq!(action, Some(OrderAction::NewOrder { qty: 1_000 }));

        // 2. Tiny tick of 10 arrives. Drift is only 10  (< 100 threshold).
        // Should NOT issue an exchange modification to protect queue priority.
        let action2 = om.on_market_trade(10, 91.0);
        assert_eq!(action2, None);

        // 3. Huge volume surge of 6,000 arrives at 2.0 mins (40% elapsed).
        // Total raw volume = 9,010. Extrapolated = 9,010 / 0.40 = 22,525 (2.25x surge).
        // Target = 1,000 * 2.2525 = 2,253 . Drift = 2,253 - 1,000 = 1,253 (> 100).
        let action3 = om.on_market_trade(6_000, 120.0);
        assert_eq!(action3, Some(OrderAction::ModifyOrder { new_qty: 2_253 }));
    }

    #[test]
    fn test_fill_processing_and_bin_reset() {
        let mut om = mock_setup();

        // Place initial order for 1,000
        om.on_market_trade(3_000, 90.0);

        // Receive partial fill of 400
        om.on_execution_fill(400);
        assert_eq!(om.working_order_qty, 600);
        assert_eq!(om.scheduler.state.remaining_qty, 77_600);

        // Transition to next bin
        om.on_bin_transition();
        assert_eq!(om.scheduler.state.current_bin, 1);
        assert_eq!(om.working_order_qty, 0);
        assert_eq!(om.raw_bin_volume, 0);
    }
}

#[cfg(test)]
mod alloc_tests {
    use super::*;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingAllocator;

    static ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            ALLOC_COUNT.fetch_add(1, Ordering::SeqCst);
            System.alloc(layout)
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            System.dealloc(ptr, layout);
        }
    }

    #[global_allocator]
    static A: CountingAllocator = CountingAllocator;

    fn mock_setup() -> OrderManager {
        let weight = 1.0 / (NUM_BINS as f64);
        let config = VwapConfig {
            parent_qty: 78_000,
            historical_weights: [weight; NUM_BINS],
            max_participation_rate: 0.50,
        };
        let expected_volumes = [10_000; NUM_BINS]; // Expect 10,000 bin
        let min_threshold = 100; // Require 100 drift to trigger exchange action

        OrderManager::new(config, expected_volumes, min_threshold)
    }
    #[test]
    fn test_zero_allocations_on_hot_path() {
        let mut om = mock_setup();

        // Capture allocation count before running hot-path operations
        let allocs_before = ALLOC_COUNT.load(Ordering::SeqCst);

        // Simulate 1,000,000 market trade ticks
        for i in 0..1_000_000 {
            let elapsed = (i % 300) as f64;
            let action = om.on_market_trade(100, elapsed);
            std::hint::black_box(action);
        }

        let allocs_after = ALLOC_COUNT.load(Ordering::SeqCst);

        // Strict assertion: exactly zero allocations must occur
        assert_eq!(
            allocs_before, allocs_after,
            "Memory was allocated during the hot path!"
        );
    }
}
