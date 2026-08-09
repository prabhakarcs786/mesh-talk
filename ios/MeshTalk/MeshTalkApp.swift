import SwiftUI

@main
struct MeshTalkApp: App {
    @StateObject private var store = MeshStore()

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(store)
        }
    }
}

/// Separated from `MeshTalkApp` so it can hold its own `@State` for driving the call
/// full-screen cover -- a plain computed `Binding` over `store.callPhase` (no local
/// `@State`) turned out to be unreliable at actually presenting the cover in testing,
/// even though the underlying call state changed correctly (the call button disabled
/// itself as expected, but the cover never appeared). Mirroring `store.callPhase` into a
/// real `@State` var via `onChange` and driving `fullScreenCover(isPresented:)` from that
/// instead fixed it.
private struct RootView: View {
    @EnvironmentObject var store: MeshStore
    @State private var isShowingCall = false

    var body: some View {
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
        .onChange(of: store.callPhase) { newPhase in
            isShowingCall = newPhase != .idle
        }
        .onAppear {
            isShowingCall = store.callPhase != .idle
        }
        .fullScreenCover(isPresented: $isShowingCall) {
            CallOverlay()
                .environmentObject(store)
        }
    }
}
