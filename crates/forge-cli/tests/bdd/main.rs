//! Cucumber harness entry point. Runs every scenario in
//! `tests/features/` serially against the compiled `forge` binary.

mod steps;
mod world;

use cucumber::World as _;
use world::BddWorld;

#[tokio::main]
async fn main() {
    BddWorld::cucumber()
        .max_concurrent_scenarios(1)
        .run_and_exit(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/features"))
        .await;
}
