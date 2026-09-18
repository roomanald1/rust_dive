use criterion::{black_box, criterion_group, criterion_main, Criterion};
use memory_ordering::vwap::{NUM_BINS, OrderManager, VwapConfig};
// Import your structs here:
// use my_crate::{OrderManager, VwapConfig, NUM_BINS};

fn mock_setup() -> OrderManager {
    let weight = 1.0 / (NUM_BINS as f64);
    let config = VwapConfig {
        parent_qty: 78_000,
        historical_vwap_curve: [weight; NUM_BINS],
        max_participation_rate: 0.50,
    };
    let expected_volumes = [10_000; NUM_BINS];
    OrderManager::new(config, expected_volumes, 100)
}

fn bench_on_market_trade(c: &mut Criterion) {
    let mut om = mock_setup();

    c.bench_function("on_market_trade_fast_path", |b| {
        b.iter(|| {
            // black_box prevents the compiler from optimizing away the call
            om.on_market_trade(black_box(3_000), black_box(3_000.0),black_box(90.0))
        })
    });
}

criterion_group!(benches, bench_on_market_trade);
criterion_main!(benches);