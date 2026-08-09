import SwiftUI

struct ChatView: View {
    @EnvironmentObject var store: MeshStore
    @State private var draft: String = ""

    var body: some View {
        VStack(spacing: 0) {
            statusBar

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        ForEach(Array(store.messages.enumerated()), id: \.offset) { index, message in
                            messageRow(message)
                                .id(index)
                        }
                    }
                    .padding()
                }
                .onChange(of: store.messages.count) { _ in
                    if let last = store.messages.indices.last {
                        withAnimation {
                            proxy.scrollTo(last, anchor: .bottom)
                        }
                    }
                }
            }

            Divider()

            HStack {
                TextField("Message", text: $draft)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(sendDraft)
                Button("Send", action: sendDraft)
                    .disabled(!store.isConnected || draft.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding()
        }
        .navigationTitle("meshtalk")
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
    }

    private func messageRow(_ message: ReceivedMessage) -> some View {
        Text(message.text)
            .padding(8)
            .background(Color.secondary.opacity(0.12))
            .clipShape(RoundedRectangle(cornerRadius: 8))
    }

    private func sendDraft() {
        store.send(draft)
        draft = ""
    }
}
