import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// One conversation with a single peer -- like opening a contact's thread in a normal
/// chat app. Shows just the messages exchanged with `peer`, with call buttons for that
/// same peer right in the header.
struct ChatThreadView: View {
    let peer: DiscoveredPeer

    @EnvironmentObject var store: MeshStore
    @State private var draft: String = ""
    @State private var pickedItem: PhotosPickerItem?
    @StateObject private var voiceRecorder = VoiceRecorder()

    private var threadMessages: [ReceivedMessage] {
        store.messages(with: peer.fullNodeId)
    }

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        ForEach(Array(threadMessages.enumerated()), id: \.offset) { index, message in
                            messageRow(message)
                                .id(index)
                        }
                        ForEach(store.activeTransferOrder, id: \.self) { transferId in
                            if let progress = store.activeTransfers[transferId] {
                                transferProgressRow(progress)
                                    .id("transfer-\(transferId)")
                            }
                        }
                    }
                    .padding()
                }
                .onChange(of: threadMessages.count) { _ in
                    if let last = threadMessages.indices.last {
                        withAnimation {
                            proxy.scrollTo(last, anchor: .bottom)
                        }
                    }
                }
                .onChange(of: store.activeTransferOrder) { order in
                    if let last = order.last {
                        withAnimation {
                            proxy.scrollTo("transfer-\(last)", anchor: .bottom)
                        }
                    }
                }
            }

            Divider()

            HStack {
                PhotosPicker(selection: $pickedItem, matching: .any(of: [.images, .videos])) {
                    Image(systemName: "paperclip")
                }
                .onChange(of: pickedItem) { newItem in
                    Task { await sendPickedItem(newItem) }
                }

                Button(action: toggleVoiceRecording) {
                    Image(systemName: voiceRecorder.isRecording ? "stop.circle.fill" : "mic")
                        .foregroundStyle(voiceRecorder.isRecording ? .red : .primary)
                }

                TextField("Message", text: $draft)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(sendDraft)
                Button("Send", action: sendDraft)
                    .disabled(!store.isConnected || draft.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding()
        }
        .navigationTitle(peer.displayName)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 2) {
                    Text(peer.displayName).font(.headline)
                    HStack(spacing: 4) {
                        Circle()
                            .fill(store.isOnline(peer.fullNodeId) ? Color.green : Color.secondary)
                            .frame(width: 6, height: 6)
                        Text(store.isOnline(peer.fullNodeId) ? "Online" : "Offline")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                        Text("\u{00B7}").foregroundStyle(.secondary)
                        securityBadge
                    }
                }
            }
            ToolbarItemGroup(placement: .navigationBarTrailing) {
                Button {
                    store.placeCall(to: peer, video: false)
                } label: {
                    Image(systemName: "phone")
                }
                .disabled(store.callPhase != .idle)

                Button {
                    store.placeCall(to: peer, video: true)
                } label: {
                    Image(systemName: "video")
                }
                .disabled(store.callPhase != .idle)
            }
        }
        .alert("Security identity changed", isPresented: identityChangedBinding) {
            Button("OK") { store.acknowledgeIdentityChange(for: peer.fullNodeId) }
        } message: {
            Text("\(peer.displayName)'s secure identity changed since you last talked. This could mean they reinstalled the app -- or it could mean someone else is impersonating them. Verify their identity again before trusting new messages.")
        }
    }

    /// "MeshTalk Direct Encryption v1" is real, per-recipient authenticated encryption
    /// once this device holds the peer's cryptographic identity -- but that's not the
    /// same as *human* identity verification (a later QR/safety-number milestone), so
    /// this deliberately says "Secure" (key ownership proven), not "Verified".
    @ViewBuilder
    private var securityBadge: some View {
        if store.identityChangedPeerIds.contains(peer.fullNodeId) {
            Label("Identity changed", systemImage: "exclamationmark.triangle.fill")
                .labelStyle(.iconOnly)
                .font(.caption2)
                .foregroundStyle(.red)
        } else if store.isSecure(peer.fullNodeId) {
            Label("Secure", systemImage: "lock.fill")
                .labelStyle(.iconOnly)
                .font(.caption2)
                .foregroundStyle(.green)
        } else {
            Label("Secure identity unavailable", systemImage: "lock.slash")
                .labelStyle(.iconOnly)
                .font(.caption2)
                .foregroundStyle(.orange)
        }
    }

    private var identityChangedBinding: Binding<Bool> {
        Binding(
            get: { store.identityChangedPeerIds.contains(peer.fullNodeId) },
            set: { if !$0 { store.acknowledgeIdentityChange(for: peer.fullNodeId) } }
        )
    }

    @ViewBuilder
    private func messageRow(_ message: ReceivedMessage) -> some View {
        let isMine = message.senderId == ownMessageSenderId
        HStack {
            if isMine { Spacer(minLength: 40) }
            Group {
                if let text = message.text {
                    Text(text)
                        .padding(8)
                        .background(isMine ? Color.accentColor.opacity(0.85) : Color.secondary.opacity(0.12))
                        .foregroundStyle(isMine ? .white : .primary)
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                } else if let attachment = message.attachment {
                    attachmentRow(attachment)
                }
            }
            if !isMine { Spacer(minLength: 40) }
        }
    }

    @ViewBuilder
    private func attachmentRow(_ attachment: FileAttachment) -> some View {
        switch attachment.kind {
        case .image:
            if let uiImage = UIImage(data: attachment.data) {
                Image(uiImage: uiImage)
                    .resizable()
                    .scaledToFit()
                    .frame(maxWidth: 220, maxHeight: 220)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            } else {
                filePlaceholder(attachment)
            }
        case .video:
            VideoAttachmentView(data: attachment.data)
        case .voice:
            VoicePlaybackView(data: attachment.data)
        case .file:
            filePlaceholder(attachment)
        }
    }

    private func filePlaceholder(_ attachment: FileAttachment) -> some View {
        HStack {
            Image(systemName: "doc")
            Text(attachment.name)
        }
        .padding(8)
        .background(Color.secondary.opacity(0.12))
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }

    private func transferProgressRow(_ progress: TransferProgressUpdate) -> some View {
        let fraction = progress.totalChunks > 0 ? Double(progress.doneChunks) / Double(progress.totalChunks) : 0
        let verb = progress.direction == .sending ? "Sending" : "Receiving"
        let kindLabel: String
        switch progress.kind {
        case .image: kindLabel = "photo"
        case .video: kindLabel = "video"
        case .voice: kindLabel = "voice note"
        case .file: kindLabel = "file"
        }
        return VStack(alignment: .leading, spacing: 4) {
            Text("\(verb) \(kindLabel)... \(Int(fraction * 100))%")
                .font(.caption)
                .foregroundStyle(.secondary)
            ProgressView(value: fraction)
                .frame(width: 160)
        }
        .padding(8)
        .background(Color.secondary.opacity(0.08))
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }

    private func sendDraft() {
        store.send(draft, to: peer.fullNodeId)
        draft = ""
    }

    private func sendPickedItem(_ item: PhotosPickerItem?) async {
        guard let item else { return }
        defer { pickedItem = nil }

        guard let data = try? await item.loadTransferable(type: Data.self) else { return }
        let isVideo = item.supportedContentTypes.contains { $0.conforms(to: .movie) }

        if isVideo {
            store.sendFile(data: data, fileName: "video.mov", mimeType: "video/quicktime", kind: .video, to: peer.fullNodeId)
        } else {
            store.sendFile(data: data, fileName: "photo.jpg", mimeType: "image/jpeg", kind: .image, to: peer.fullNodeId)
        }
    }

    private func toggleVoiceRecording() {
        if voiceRecorder.isRecording {
            if let data = voiceRecorder.stopRecording() {
                store.sendFile(data: data, fileName: "voice.m4a", mimeType: "audio/m4a", kind: .voice, to: peer.fullNodeId)
            }
        } else {
            voiceRecorder.startRecording()
        }
    }
}
