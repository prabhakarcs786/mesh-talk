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

    private var client: MeshClient?
    private var pollTimer: Timer?

    /// Starts (or restarts) a mesh node with the given settings.
    ///
    /// - `peerAddrs`: directly-reachable peers on the same Wi-Fi network, e.g.
    ///   `"192.168.1.42:9001"`. This is today's UDP transport; it will be replaceable with
    ///   Bluetooth LE auto-discovery once peripheral mode lands (see the repo roadmap).
    func connect(displayName: String, listenPort: UInt16, peerAddrs: [String], channel: String) {
        disconnect()
        do {
            let newClient = try MeshClient(
                displayName: displayName,
                listenAddr: "0.0.0.0:\(listenPort)",
                peerAddrs: peerAddrs,
                channelPassphrase: channel,
                ttl: 16
            )
            client = newClient
            nodeId = newClient.nodeId()
            isConnected = true
            lastError = nil
            startPolling()
        } catch {
            lastError = "\(error)"
            isConnected = false
        }
    }

    func disconnect() {
        pollTimer?.invalidate()
        pollTimer = nil
        client = nil
        isConnected = false
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

    private func drainInbox() {
        guard let client else { return }
        while let message = client.pollMessage() {
            messages.append(message)
        }
    }
}
