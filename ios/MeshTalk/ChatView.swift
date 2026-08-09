import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

struct ChatView: View {
    @EnvironmentObject var store: MeshStore
    @State private var draft: String = ""
    @State private var pickedItem: PhotosPickerItem?
    @StateObject private var voiceRecorder = VoiceRecorder()

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

    @ViewBuilder
    private func messageRow(_ message: ReceivedMessage) -> some View {
        if let text = message.text {
            Text(text)
                .padding(8)
                .background(Color.secondary.opacity(0.12))
                .clipShape(RoundedRectangle(cornerRadius: 8))
        } else if let attachment = message.attachment {
            attachmentRow(attachment)
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

    private func sendDraft() {
        store.send(draft)
        draft = ""
    }

    private func sendPickedItem(_ item: PhotosPickerItem?) async {
        guard let item else { return }
        defer { pickedItem = nil }

        guard let data = try? await item.loadTransferable(type: Data.self) else { return }
        let isVideo = item.supportedContentTypes.contains { $0.conforms(to: .movie) }

        if isVideo {
            store.sendFile(data: data, fileName: "video.mov", mimeType: "video/quicktime", kind: .video)
        } else {
            store.sendFile(data: data, fileName: "photo.jpg", mimeType: "image/jpeg", kind: .image)
        }
    }

    private func toggleVoiceRecording() {
        if voiceRecorder.isRecording {
            if let data = voiceRecorder.stopRecording() {
                store.sendFile(data: data, fileName: "voice.m4a", mimeType: "audio/m4a", kind: .voice)
            }
        } else {
            voiceRecorder.startRecording()
        }
    }
}
