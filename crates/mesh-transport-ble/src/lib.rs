//! Bluetooth LE Transport (central role only) for `mesh-core`.
//!
//! # Important limitation -- read before use
//!
//! [`btleplug`](https://docs.rs/btleplug), the cross-platform BLE library this is built
//! on, only implements the BLE **central** role: scanning for and connecting to
//! peripherals. It has no API for **advertising as a peripheral** or running a GATT
//! server -- every OS treats that as a separate role with its own native API
//! (CoreBluetooth `CBPeripheralManager` on macOS/iOS, `BluetoothGattServer` +
//! `BluetoothLeAdvertiser` on Android, BlueZ's GATT application API on Linux).
//!
//! Practically, that means this crate lets a meshtalk node **find and read from** other
//! nodes that are already advertising the meshtalk GATT service and running a GATT
//! server -- but it does not yet make a node **discoverable itself**. Two nodes running
//! only this crate cannot fully relay for each other until a peripheral/advertising
//! counterpart exists per platform. That work is tracked as separate follow-up issues
//! (see the repo issue tracker) since it requires native platform code, not just more
//! cross-platform Rust.
//!
//! What *is* real and working here: adapter/manager setup, scanning filtered to the
//! meshtalk service UUID, connecting, GATT characteristic discovery, chunked
//! send/reassembly framing, and wiring all of that into `mesh_core::Transport`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::StreamExt;
use mesh_core::Transport;
use tokio::sync::{mpsc, Mutex};
use uuid::{uuid, Uuid};

/// Custom 128-bit UUID identifying the meshtalk GATT service. Any peripheral advertising
/// this UUID is assumed to speak the meshtalk protocol.
pub const MESHTALK_SERVICE_UUID: Uuid = uuid!("6d657368-7461-6c6b-0000-000073766301");

/// Characteristic used to exchange framed, chunked message bytes.
pub const MESHTALK_DATA_CHARACTERISTIC_UUID: Uuid = uuid!("6d657368-7461-6c6b-0000-000064617461");

/// Conservative chunk size, safely below the default negotiated ATT MTU payload on most
/// platforms/devices (which can vary from ~20 to ~500 bytes).
const MAX_CHUNK_LEN: usize = 180;

/// BLE `Transport` implementation covering the central (scan + connect) role.
pub struct BleCentralTransport {
    adapter: Adapter,
    peers: Mutex<HashMap<String, Peripheral>>,
    incoming_tx: mpsc::UnboundedSender<Vec<u8>>,
    incoming_rx: Mutex<mpsc::UnboundedReceiver<Vec<u8>>>,
}

impl BleCentralTransport {
    pub async fn new() -> anyhow::Result<Arc<Self>> {
        let manager = Manager::new().await?;
        let adapter = manager
            .adapters()
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no Bluetooth adapter found on this device"))?;

        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel();

        let transport = Arc::new(Self {
            adapter,
            peers: Mutex::new(HashMap::new()),
            incoming_tx,
            incoming_rx: Mutex::new(incoming_rx),
        });

        transport
            .adapter
            .start_scan(ScanFilter {
                services: vec![MESHTALK_SERVICE_UUID],
            })
            .await?;

        let watcher = transport.clone();
        tokio::spawn(async move {
            watcher.watch_for_peers().await;
        });

        Ok(transport)
    }

    /// Polls for nearby peripherals advertising the meshtalk service and connects to any
    /// new ones. A real implementation would react to `adapter.events()` instead of
    /// polling; polling keeps this first version simple.
    async fn watch_for_peers(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;

            let Ok(peripherals) = self.adapter.peripherals().await else {
                continue;
            };

            for peripheral in peripherals {
                let Ok(Some(props)) = peripheral.properties().await else {
                    continue;
                };
                if !props.services.contains(&MESHTALK_SERVICE_UUID) {
                    continue;
                }

                // Use the platform-opaque PeripheralId, not the BD address: macOS
                // CoreBluetooth always reports 00:00:00:00:00:00 for `address` (Apple
                // hides real MAC addresses from apps for privacy), which would make
                // every macOS peer collide under the same key.
                let addr = peripheral.id().to_string();
                if self.peers.lock().await.contains_key(&addr) {
                    continue; // already connected
                }

                if let Err(e) = self.connect_and_subscribe(addr.clone(), peripheral).await {
                    eprintln!("[mesh-transport-ble] failed to connect to {addr}: {e}");
                }
            }
        }
    }

    async fn connect_and_subscribe(&self, addr: String, peripheral: Peripheral) -> anyhow::Result<()> {
        peripheral.connect().await?;
        peripheral.discover_services().await?;

        let data_char = peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == MESHTALK_DATA_CHARACTERISTIC_UUID)
            .ok_or_else(|| anyhow::anyhow!("peer {addr} is missing the meshtalk data characteristic"))?;

        peripheral.subscribe(&data_char).await?;
        let mut notifications = peripheral.notifications().await?;

        self.peers.lock().await.insert(addr, peripheral);

        let incoming_tx = self.incoming_tx.clone();
        tokio::spawn(async move {
            // Notifications may arrive as several chunks per logical message; reassemble
            // using a 4-byte big-endian length prefix written by the sender.
            let mut buf: Vec<u8> = Vec::new();
            while let Some(notification) = notifications.next().await {
                buf.extend_from_slice(&notification.value);
                while buf.len() >= 4 {
                    let len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
                    if buf.len() < 4 + len {
                        break; // wait for more chunks
                    }
                    let frame = buf[4..4 + len].to_vec();
                    buf.drain(0..4 + len);
                    let _ = incoming_tx.send(frame);
                }
            }
        });

        Ok(())
    }
}

#[async_trait]
impl Transport for BleCentralTransport {
    async fn send_to_peer(&self, peer: &str, bytes: Vec<u8>) -> anyhow::Result<()> {
        let peripheral = self
            .peers
            .lock()
            .await
            .get(peer)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("peer {peer} is not currently connected"))?;

        let data_char = peripheral
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == MESHTALK_DATA_CHARACTERISTIC_UUID)
            .ok_or_else(|| anyhow::anyhow!("peer {peer} is missing the meshtalk data characteristic"))?;

        let mut framed = Vec::with_capacity(4 + bytes.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        framed.extend_from_slice(&bytes);

        for chunk in framed.chunks(MAX_CHUNK_LEN) {
            peripheral
                .write(&data_char, chunk, WriteType::WithoutResponse)
                .await?;
        }
        Ok(())
    }

    async fn recv(&self) -> anyhow::Result<Vec<u8>> {
        self.incoming_rx
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("BLE incoming channel closed"))
    }

    fn peers(&self) -> Vec<String> {
        // `Transport::peers` is synchronous, so use try_lock: under contention we just
        // return an empty/stale list for this one call, which is fine since flooding
        // re-broadcasts on every new message anyway.
        self.peers
            .try_lock()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    }
}
