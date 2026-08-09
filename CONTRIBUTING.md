# Contributing to meshtalk

Thanks for considering a contribution. This project builds an offline, internet-free
mesh chat engine — the core hard problems are in `crates/mesh-core` (routing, crypto,
de-duplication) and in adding real radio transports (Bluetooth LE, Wi-Fi Direct, LoRa).

## Getting started

```bash
git clone https://github.com/prabhakarcs786/mesh-talk.git
cd mesh-talk
cargo build
cargo test
```

Try the 3-node relay demo described in [README.md](README.md#try-the-relay-demo-3-nodes-no-real-radios-needed)
to see the mesh routing working end-to-end before diving into the code.

## Project layout

```
crates/
  mesh-core/           radio-agnostic engine: identity, crypto, routing, message store
  mesh-transport-udp/  Transport impl over UDP (for local dev/testing)
  mesh-cli/            terminal demo client
```

If you're adding a new radio transport (Bluetooth LE, Wi-Fi Direct, LoRa), implement the
`Transport` trait in `crates/mesh-core/src/transport.rs` — you shouldn't need to touch the
routing/crypto logic in `node.rs` at all. That separation is intentional; please keep it
that way in PRs.

## How to contribute

1. Check open issues labeled [`good first issue`](https://github.com/prabhakarcs786/mesh-talk/labels/good%20first%20issue)
   if you're not sure where to start.
2. Open an issue before starting on a large change (new transport, protocol change) so we
   can agree on the approach first. Small fixes/tests can go straight to a PR.
3. Fork, branch, and submit a PR against `main`. Keep PRs focused — one logical change per
   PR is easier to review than a large mixed one.
4. Make sure `cargo test` and `cargo clippy` pass before opening the PR.

## Code style

- Run `cargo fmt` before committing.
- Prefer small, well-named functions with doc comments explaining *why*, not just *what*
  (see existing modules in `mesh-core` for the expected level of detail).
- New protocol-level behavior (routing, crypto, wire format) should include unit tests.

## Reporting bugs / proposing features

Open a GitHub issue. For bugs, include your OS, Rust version (`rustc --version`), and
steps to reproduce. For features, describe the use case (e.g., "disaster response team
needs X") so we can evaluate the trade-offs (see the Roadmap in the README for what's
already planned).

## Code of conduct

Be respectful and constructive. This project exists to help people communicate when
infrastructure fails them — keep that spirit in how you engage with other contributors.
