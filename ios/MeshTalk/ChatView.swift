import SwiftUI

/// Home screen for the Chat tab: one card per connected device, like a normal chat app's
/// conversation list -- not a single shared feed. Tapping a card opens that person's
/// `ChatThreadView`.
struct ChatView: View {
    @EnvironmentObject var store: MeshStore

    private var conversations: [DiscoveredPeer] {
        store.connectedPeers.values.sorted { $0.displayName.localizedCaseInsensitiveCompare($1.displayName) == .orderedAscending }
    }

    var body: some View {
        VStack(spacing: 0) {
            statusBar

            if conversations.isEmpty {
                emptyState
            } else {
                List(conversations, id: \.fullNodeId) { peer in
                    NavigationLink {
                        ChatThreadView(peer: peer)
                    } label: {
                        conversationRow(peer)
                    }
                }
                .listStyle(.plain)
            }
        }
        .navigationTitle("Chats")
    }

    private var statusBar: some View {
        HStack {
            Circle()
                .fill(store.isConnected ? Color.green : Color.red)
                .frame(width: 8, height: 8)
            Text(store.isConnected ? "connected -- id \(store.nodeId)" : "not connected")
                .font(.caption)
                .foregroundStyle(.secondary)
            Spacer()
        }
        .padding(.horizontal)
        .padding(.top, 8)
        .padding(.bottom, 4)
    }

    private var emptyState: some View {
        VStack(spacing: 12) {
            Spacer()
            Image(systemName: "message")
                .font(.system(size: 40))
                .foregroundStyle(.secondary)
            Text("No conversations yet")
                .font(.headline)
            Text("Connect to a nearby device in Settings to start chatting.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 40)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func conversationRow(_ peer: DiscoveredPeer) -> some View {
        HStack(spacing: 12) {
            ZStack {
                Circle()
                    .fill(Color.accentColor.opacity(0.2))
                    .frame(width: 44, height: 44)
                Text(String(peer.displayName.prefix(1)).uppercased())
                    .font(.headline)
                    .foregroundStyle(Color.accentColor)
            }

            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(peer.displayName)
                        .font(.body.weight(.medium))
                    Circle()
                        .fill(store.isOnline(peer.fullNodeId) ? Color.green : Color.secondary.opacity(0.4))
                        .frame(width: 7, height: 7)
                }
                Text(store.lastMessagePreview(with: peer.fullNodeId) ?? "Say hello \u{1F44B}")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer()

            Text(store.isOnline(peer.fullNodeId) ? "Online" : "Offline")
                .font(.caption2)
                .foregroundStyle(store.isOnline(peer.fullNodeId) ? .green : .secondary)
        }
        .padding(.vertical, 4)
    }
}
