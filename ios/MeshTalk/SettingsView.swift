import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var store: MeshStore

    @AppStorage("meshtalk.displayName") private var displayName: String = ""
    @AppStorage("meshtalk.listenPort") private var listenPort: Int = 9001
    @AppStorage("meshtalk.channel") private var channel: String = "mesh-demo"
    @State private var manualAddress: String = ""
    @State private var chatSheetPeer: DiscoveredPeer?

    var body: some View {
        Form {
            Section("Identity") {
                TextField("Display name", text: $displayName)
            }

            Section("Network") {
                Stepper("Listen port: \(listenPort)", value: $listenPort, in: 1024...65535)
                TextField("Channel passphrase", text: $channel)
            }

            if let error = store.lastError {
                Section {
                    Text(error).foregroundStyle(.red)
                }
            }

            Section {
                Button(store.isConnected ? "Restart" : "Start") {
                    store.start(displayName: displayName, listenPort: UInt16(listenPort), channel: channel)
                }
                .disabled(displayName.trimmingCharacters(in: .whitespaces).isEmpty)

                if store.isConnected {
                    Button("Stop", role: .destructive) {
                        store.disconnect()
                    }
                }
            }

            if store.isConnected {
                Section {
                    if store.discoveredPeers.isEmpty {
                        HStack {
                            ProgressView()
                            Text("Looking for nearby devices...")
                                .foregroundStyle(.secondary)
                        }
                    } else {
                        ForEach(store.discoveredPeers, id: \.address) { peer in
                            nearbyPeerRow(peer)
                        }
                    }
                } header: {
                    Text("Nearby devices")
                } footer: {
                    Text("Found automatically on your Wi-Fi network, like Bluetooth pairing -- no IP address needed. Compare the code shown here with the one on the other device before connecting.")
                }

                Section {
                    TextField("IP:port (e.g. 192.168.1.42:9001)", text: $manualAddress)
                    Button("Connect manually") {
                        store.connectManually(address: manualAddress)
                        manualAddress = ""
                    }
                    .disabled(manualAddress.trimmingCharacters(in: .whitespaces).isEmpty)
                } header: {
                    Text("Advanced")
                } footer: {
                    Text("Only needed if a device isn't on the same local network as auto-discovery.")
                }
            }
        }
        .navigationTitle("Settings")
        .sheet(item: $chatSheetPeer) { peer in
            NavigationStack {
                ChatThreadView(peer: peer)
                    .environmentObject(store)
                    .toolbar {
                        ToolbarItem(placement: .cancellationAction) {
                            Button("Done") { chatSheetPeer = nil }
                        }
                    }
            }
        }
    }

    private func nearbyPeerRow(_ peer: DiscoveredPeer) -> some View {
        let isConnected = store.connectedAddresses.contains(peer.address)
        return HStack {
            VStack(alignment: .leading) {
                Text(peer.displayName).font(.body)
                Text("code \(peer.pairingCode)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            if isConnected {
                Button {
                    chatSheetPeer = peer
                } label: {
                    Image(systemName: "message.fill")
                }
                .buttonStyle(.borderless)

                Button {
                    store.placeCall(to: peer, video: false)
                } label: {
                    Image(systemName: "phone.fill")
                }
                .buttonStyle(.borderless)
                .disabled(store.callPhase != .idle)

                Button {
                    store.placeCall(to: peer, video: true)
                } label: {
                    Image(systemName: "video.fill")
                }
                .buttonStyle(.borderless)
                .disabled(store.callPhase != .idle)

                Label("Connected", systemImage: "checkmark.circle.fill")
                    .labelStyle(.iconOnly)
                    .foregroundStyle(.green)
            } else {
                Button("Connect") {
                    store.connect(to: peer)
                }
            }
        }
    }
}
