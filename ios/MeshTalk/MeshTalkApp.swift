import SwiftUI

@main
struct MeshTalkApp: App {
    @StateObject private var store = MeshStore()

    var body: some Scene {
        WindowGroup {
            TabView {
                NavigationStack {
                    ChatView()
                }
                .tabItem {
                    Label("Chat", systemImage: "message")
                }

                NavigationStack {
                    SettingsView()
                }
                .tabItem {
                    Label("Settings", systemImage: "gear")
                }
            }
            .environmentObject(store)
        }
    }
}
