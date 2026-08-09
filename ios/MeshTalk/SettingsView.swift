import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var store: MeshStore

    @AppStorage("meshtalk.displayName") private var displayName: String = ""
    @AppStorage("meshtalk.listenPort") private var listenPort: Int = 9001
    @AppStorage("meshtalk.channel") private var channel: String = "mesh-demo"
    @State private var peerAddrsText: String = ""

    var body: some View {
        Form {
            Section("Identity") {
                TextField("Display name", text: $displayName)
            }

            Section("Network") {
                Stepper("Listen port: \(listenPort)", value: $listenPort, in: 1024...65535)
                TextField("Channel passphrase", text: $channel)
            }

            Section {
                TextField("Peer addresses (comma-separated, e.g. 192.168.1.42:9001)", text: $peerAddrsText, axis: .vertical)
            } header: {
                Text("Peers")
            } footer: {
                Text("Today's transport is Wi-Fi/UDP-based, so list the IP:port of devices on the same network you want to relay with directly. Bluetooth LE auto-discovery (no manual addresses needed) is on the roadmap -- see the repo issue tracker.")
            }

            if let error = store.lastError {
                Section {
                    Text(error).foregroundStyle(.red)
                }
            }

            Section {
                Button(store.isConnected ? "Reconnect" : "Connect") {
                    connect()
                }
                .disabled(displayName.trimmingCharacters(in: .whitespaces).isEmpty)

                if store.isConnected {
                    Button("Disconnect", role: .destructive) {
                        store.disconnect()
                    }
                }
            }
        }
        .navigationTitle("Settings")
    }

    private func connect() {
        let peers = peerAddrsText
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }

        store.connect(
            displayName: displayName,
            listenPort: UInt16(listenPort),
            peerAddrs: peers,
            channel: channel
        )
    }
}
