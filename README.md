# meshtalk

Offline, internet-free mesh chat. Devices relay messages hop-by-hop through each other
so two people who are *not* directly in range of each other can still reach one another,
as long as there's an unbroken chain of nodes between them (Bluetooth/Wi-Fi Direct/LoRa
range today, no cellular network or internet required).

This is not a wrapper around a cloud API. The engineering is in the mesh routing layer
itself: flood routing with a TTL hop budget, message de-duplication, and end-to-end
authenticated encryption that survives relaying through untrusted intermediate nodes.

## Status

Early stage. What works today:
- `mesh-core`: transport-agnostic engine (identity, crypto, flood routing, de-dup, TTL relay)
- `mesh-transport-udp`: a UDP-based `Transport` impl for testing on a LAN/loopback without
  real radio hardware
- `mesh-cli`: a terminal chat client to exercise the engine end-to-end

Not built yet (see Roadmap): real short-range radio transports (Bluetooth LE / Wi-Fi
Direct) for phones, a mobile app, async voice/video "store-and-forward" messages, and a
browser client.

## Architecture

```
crates/
  mesh-core/           radio-agnostic engine: identity, crypto, routing, message store
  mesh-transport-udp/  Transport impl over UDP (for local dev/testing)
  mesh-transport-ble/  Transport impl over Bluetooth LE (central role only -- see below)
  mesh-cli/            terminal demo client
```

- **Identity**: each node has an Ed25519 keypair; the public key is its `NodeId`. Every
  message is signed so any relay or recipient can verify who originally sent it.
- **Encryption**: messages are encrypted with a shared channel key (XChaCha20-Poly1305,
  key derived from a passphrase via BLAKE3) — like tuning into the same walkie-talkie
  frequency. Private 1:1 key-exchange encryption is on the roadmap.
- **Routing**: flood routing with a TTL hop budget. Every node that receives a new
  message (checked via a bounded de-dup cache) decrements the TTL and re-broadcasts it to
  its own directly-reachable peers, then attempts to decrypt it for display. This is what
  lets a message travel across a whole chain of relays, exactly like a bucket brigade.
- **Transport**: a `Transport` trait decouples routing from the physical link. Swapping
  in Bluetooth LE or Wi-Fi Direct on mobile means implementing this one trait — the
  routing/crypto logic above doesn't change.

## Try the relay demo (3 nodes, no real radios needed)

This simulates three people in a line, 1 hop apart, where the two end nodes are **not**
directly reachable — the middle node relays for them.

```bash
cargo build --release

# terminal 1 (alice can only reach bob)
./target/release/mesh-cli --name alice --listen 127.0.0.1:9001 --peers 127.0.0.1:9002

# terminal 2 (bob can reach both alice and carol -- the relay)
./target/release/mesh-cli --name bob --listen 127.0.0.1:9002 --peers 127.0.0.1:9001,127.0.0.1:9003

# terminal 3 (carol can only reach bob)
./target/release/mesh-cli --name carol --listen 127.0.0.1:9003 --peers 127.0.0.1:9002
```

Type a message in alice's terminal and it will show up in carol's terminal, relayed
through bob, even though alice and carol never talk to each other directly.

## Roadmap

1. Real short-range radio transports: Bluetooth LE and Wi-Fi Direct (mobile), LoRa
   (long-range, low-bandwidth) for text/telemetry.

   **BLE status**: `mesh-transport-ble` implements the central (scan + connect) role via
   [`btleplug`](https://docs.rs/btleplug), verified against this machine's real Bluetooth
   adapter (see `crates/mesh-transport-ble/examples/scan_nearby.rs`). `btleplug` has no
   peripheral/advertising API, though, so a node can't yet make itself discoverable --
   that half needs native platform code (CoreBluetooth `CBPeripheralManager` on
   macOS/iOS, `BluetoothGattServer`/`BluetoothLeAdvertiser` on Android, BlueZ's GATT
   application API on Linux). Tracked as follow-up issues per platform.
2. Mobile app (iOS/Android) via Rust core + FFI bindings (UniFFI), since Xcode/Android
   tooling can call into `mesh-core` directly without rewriting the engine.
3. Async voice/video "messages" (store-and-forward, like voice notes) — realistic over
   many hops, unlike live calls which need low latency and higher bandwidth.
4. Live voice calls limited to a small number of hops over Wi-Fi Direct.
5. Optional browser client (experimental — Web Bluetooth/WebRTC have limited/no support
   on iOS Safari, so this will always be a secondary option to the native mobile app).

## Why this matters

Built for places with no cellular signal or internet: disaster response, remote/rural
areas, hiking/expeditions, and censorship-resistant communication.

## License

MIT
