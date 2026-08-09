import Foundation

/// Thin observable wrapper around the Rust `MeshClient`, polling for incoming messages on
/// a timer since UniFFI's `pollMessage()` is a plain non-blocking call, not a stream.
/// `MeshClient`/`ReceivedMessage` come from the generated `mesh_mobile.swift` file, which
/// is compiled directly into this same app target (no import needed).
@MainActor
final class MeshStore: ObservableObject {
    @Published var messages: [ReceivedMessage] = []
    @Published var isConnected = false
    @Published var nodeId: String = ""
    @Published var lastError: String?
    /// Nearby devices found automatically on the local Wi-Fi network -- like a
    /// Bluetooth/Wi-Fi device picker, no IP address typing required.
    @Published var discoveredPeers: [DiscoveredPeer] = []
    @Published var connectedAddresses: Set<String> = []

    private var client: MeshClient?
    private var pollTimer: Timer?
    private var discoveryTimer: Timer?

    /// Starts a mesh node and immediately starts LAN auto-discovery so nearby devices
    /// running meshtalk on the same Wi-Fi network show up on their own -- connecting to
    /// one is then a single tap (`connect(to:)`) instead of typing an IP address.
    func start(displayName: String, listenPort: UInt16, channel: String) {
        disconnect()
        do {
            let newClient = try MeshClient(
                displayName: displayName,
                listenAddr: "0.0.0.0:\(listenPort)",
                peerAddrs: [],
                channelPassphrase: channel,
                ttl: 16
            )
            try newClient.startDiscovery()
            client = newClient
            nodeId = newClient.nodeId()
            isConnected = true
            lastError = nil
            startPolling()
            startDiscoveryPolling()
        } catch {
            lastError = "\(error)"
            isConnected = false
        }
    }

    /// One-tap connect to a device found via auto-discovery -- no manual IP entry.
    func connect(to peer: DiscoveredPeer) {
        client?.addPeer(address: peer.address)
        connectedAddresses.insert(peer.address)
    }

    /// Fallback for when auto-discovery doesn't find a peer (e.g. different subnet):
    /// still supported, just not the primary path anymore.
    func connectManually(address: String) {
        guard !address.trimmingCharacters(in: .whitespaces).isEmpty else { return }
        client?.addPeer(address: address)
        connectedAddresses.insert(address)
    }

    func disconnect() {
        pollTimer?.invalidate()
        pollTimer = nil
        discoveryTimer?.invalidate()
        discoveryTimer = nil
        client = nil
        isConnected = false
        discoveredPeers = []
        connectedAddresses = []
    }

    func send(_ text: String) {
        guard let client, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        if !client.send(text: text) {
            lastError = "Failed to send -- no reachable peers right now."
        }
    }

    private func startPolling() {
        pollTimer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            self?.drainInbox()
        }
    }

    private func startDiscoveryPolling() {
        discoveryTimer = Timer.scheduledTimer(withTimeInterval: 1.5, repeats: true) { [weak self] _ in
            self?.refreshDiscoveredPeers()
        }
    }

    private func refreshDiscoveredPeers() {
        guard let client else { return }
        discoveredPeers = client.discoveredPeers()
    }

    private func drainInbox() {
        guard let client else { return }
        while let message = client.pollMessage() {
            messages.append(message)
        }
    }
}
