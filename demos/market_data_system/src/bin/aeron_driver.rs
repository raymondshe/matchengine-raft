//! Standalone Aeron media driver for the Linux external-driver validation.
//!
//! This is the small Rust equivalent of `aeronmd`: it owns the driver loop and
//! reads `AERON_DIR` (plus the usual `AERON_*` settings). Run it from the
//! market-data demo crate with `--features aeron`:
//!
//! ```shell
//! AERON_DIR=/dev/shm/aeron-$USER \
//!   cargo run --release --manifest-path demos/market_data_system/Cargo.toml \
//!     --features aeron --bin aeron_driver
//! ```
//!
//! The engine and service clients must point at the same `AERON_DIR`; no
//! embedded driver is started on either side.

use ctrlc::set_handler;
use rusteron_media_driver::AeronCError;
use rusteron_media_driver::AeronDriver;
use rusteron_media_driver::AeronDriverContext;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let running = Arc::new(AtomicBool::new(true));
    let running_clone = Arc::clone(&running);
    set_handler(move || {
        running_clone.store(false, Ordering::SeqCst);
    })?;

    let context = AeronDriverContext::new()?;
    // Match aeronmd: apply AERON_DRIVER_CPUSET_AFFINITY and the AERON_*_CPU_AFFINITY
    // settings before the driver starts its agents.
    context.apply_cgroup_cpuset_affinity()?;
    context.set_thread_affinity_on_start();
    eprintln!("aeron media driver dir: {:?}", context.get_dir());
    context.print_configuration();

    let driver = AeronDriver::new(&context).map_err(AeronCError::capture_errmsg)?;
    driver.start(true).map_err(AeronCError::capture_errmsg)?;
    eprintln!("aeron media driver started; press Ctrl-C to stop");

    while running.load(Ordering::Acquire) {
        let work = driver.main_do_work().map_err(AeronCError::capture_errmsg)?;
        driver.main_idle_strategy(work);
    }

    eprintln!("aeron media driver stopped");
    Ok(())
}
