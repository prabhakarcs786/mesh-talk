//! Proves the crate can talk to the real local Bluetooth adapter: scans for any nearby
//! BLE devices (not just meshtalk peers, since nothing advertises that service yet) and
//! prints what it finds. Run with: `cargo run -p mesh-transport-ble --example scan_nearby`
//!
//! macOS will prompt for Bluetooth permission the first time this runs from a terminal.

use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::Manager;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let manager = Manager::new().await?;
    let adapter = manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no Bluetooth adapter found"))?;

    println!("scanning for nearby BLE devices for 5 seconds...");
    adapter.start_scan(ScanFilter::default()).await?;
    tokio::time::sleep(Duration::from_secs(5)).await;

    let peripherals = adapter.peripherals().await?;
    if peripherals.is_empty() {
        println!("no BLE devices found (try enabling Bluetooth / moving closer to a device).");
    }
    for peripheral in peripherals {
        if let Some(props) = peripheral.properties().await? {
            let name = props.local_name.unwrap_or_else(|| "(unnamed)".to_string());
            // Note: `props.address` is masked to 00:00:00:00:00:00 on macOS (Apple hides
            // real BD addresses from apps for privacy) -- `peripheral.id()` is the
            // portable, collision-free identifier to use instead, which is what
            // `BleCentralTransport` keys its peer map by.
            println!(
                "- {} id={} addr={} rssi={:?}",
                name,
                peripheral.id(),
                props.address,
                props.rssi
            );
        }
    }

    Ok(())
}
