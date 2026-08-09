import AVFoundation
import AVKit
import SwiftUI

/// Records short voice notes to a temporary AAC file and hands back the raw bytes to
/// send, since `MeshClient.sendFile` just takes `Data`.
@MainActor
final class VoiceRecorder: NSObject, ObservableObject {
    @Published var isRecording = false

    private var recorder: AVAudioRecorder?
    private var fileURL: URL?

    func startRecording() {
        let session = AVAudioSession.sharedInstance()
        try? session.setCategory(.playAndRecord, mode: .default)
        try? session.setActive(true)

        let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".m4a")
        let settings: [String: Any] = [
            AVFormatIDKey: kAudioFormatMPEG4AAC,
            AVSampleRateKey: 12_000,
            AVNumberOfChannelsKey: 1,
            AVEncoderAudioQualityKey: AVAudioQuality.medium.rawValue,
        ]

        guard let recorder = try? AVAudioRecorder(url: url, settings: settings) else { return }
        self.recorder = recorder
        fileURL = url
        recorder.record()
        isRecording = true
    }

    /// Stops recording and returns the recorded audio bytes, or `nil` if nothing usable
    /// was recorded.
    func stopRecording() -> Data? {
        recorder?.stop()
        isRecording = false
        guard let fileURL, let data = try? Data(contentsOf: fileURL) else { return nil }
        try? FileManager.default.removeItem(at: fileURL)
        self.fileURL = nil
        return data
    }
}

/// Play/pause button for a received voice note.
struct VoicePlaybackView: View {
    let data: Data

    @State private var player: AVAudioPlayer?
    @State private var isPlaying = false

    var body: some View {
        Button(action: togglePlayback) {
            HStack {
                Image(systemName: isPlaying ? "pause.circle.fill" : "play.circle.fill")
                Text("Voice note")
            }
        }
        .padding(8)
        .background(Color.secondary.opacity(0.12))
        .clipShape(RoundedRectangle(cornerRadius: 8))
    }

    private func togglePlayback() {
        if isPlaying {
            player?.stop()
            isPlaying = false
            return
        }
        guard let newPlayer = try? AVAudioPlayer(data: data) else { return }
        player = newPlayer
        newPlayer.play()
        isPlaying = true
    }
}

/// Writes received video bytes to a temp file (AVPlayer needs a URL, not raw bytes) and
/// shows an inline player.
struct VideoAttachmentView: View {
    let data: Data

    @State private var fileURL: URL?

    var body: some View {
        Group {
            if let fileURL {
                VideoPlayerView(url: fileURL)
                    .frame(width: 220, height: 160)
                    .clipShape(RoundedRectangle(cornerRadius: 8))
            } else {
                ProgressView()
                    .frame(width: 220, height: 160)
            }
        }
        .onAppear {
            guard fileURL == nil else { return }
            let url = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString + ".mov")
            try? data.write(to: url)
            fileURL = url
        }
    }
}

private struct VideoPlayerView: View {
    let url: URL
    var body: some View {
        VideoPlayer(player: AVPlayer(url: url))
    }
}
