import SwiftUI

/// Shown as a full-screen cover whenever `store.callPhase != .idle` -- an incoming-call
/// banner, an outgoing "ringing" screen, or the active in-call screen (audio or video).
struct CallOverlay: View {
    @EnvironmentObject var store: MeshStore

    var body: some View {
        switch store.callPhase {
        case .idle:
            EmptyView()
        case let .incomingRinging(_, name, _, video):
            IncomingCallView(name: name, video: video)
        case let .outgoingRinging(_, name, _, video):
            InCallView(name: name, video: video, isRinging: true, startedAt: nil)
        case let .active(_, name, _, video, startedAt):
            InCallView(name: name, video: video, isRinging: false, startedAt: startedAt)
        }
    }
}

private struct IncomingCallView: View {
    @EnvironmentObject var store: MeshStore
    let name: String
    let video: Bool

    var body: some View {
        VStack(spacing: 32) {
            Spacer()
            Image(systemName: video ? "video.fill" : "phone.fill")
                .font(.system(size: 56))
            Text(name).font(.title)
            Text(video ? "Incoming video call..." : "Incoming call...")
                .foregroundStyle(.secondary)
            Spacer()
            HStack(spacing: 60) {
                Button {
                    store.rejectIncomingCall()
                } label: {
                    Image(systemName: "phone.down.fill")
                        .font(.system(size: 28))
                        .padding(20)
                        .background(Circle().fill(Color.red))
                        .foregroundStyle(.white)
                }
                Button {
                    store.acceptIncomingCall()
                } label: {
                    Image(systemName: "phone.fill")
                        .font(.system(size: 28))
                        .padding(20)
                        .background(Circle().fill(Color.green))
                        .foregroundStyle(.white)
                }
            }
            .padding(.bottom, 60)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black.opacity(0.92))
        .foregroundStyle(.white)
    }
}

private struct InCallView: View {
    @EnvironmentObject var store: MeshStore
    let name: String
    let video: Bool
    let isRinging: Bool
    let startedAt: Date?

    @State private var elapsedText = "00:00"
    private let timer = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    var body: some View {
        ZStack {
            if video {
                RemoteVideoView(frameData: store.remoteVideoFrame)
                    .ignoresSafeArea()
                    .background(Color.black)
            } else {
                Color.black.ignoresSafeArea()
            }

            VStack {
                Spacer()
                Text(name).font(.title).foregroundStyle(.white)
                Text(isRinging ? "Ringing..." : elapsedText)
                    .foregroundStyle(.white.opacity(0.8))
                Spacer()

                if video, let capture = store.callVideo {
                    LocalVideoPreview(capture: capture)
                        .frame(width: 110, height: 150)
                        .clipShape(RoundedRectangle(cornerRadius: 12))
                        .padding()
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }

                HStack(spacing: 40) {
                    Button {
                        store.toggleMute()
                    } label: {
                        Image(systemName: store.isMuted ? "mic.slash.fill" : "mic.fill")
                            .font(.system(size: 22))
                            .padding(18)
                            .background(Circle().fill(Color.white.opacity(0.2)))
                            .foregroundStyle(.white)
                    }
                    Button {
                        store.hangUp()
                    } label: {
                        Image(systemName: "phone.down.fill")
                            .font(.system(size: 22))
                            .padding(18)
                            .background(Circle().fill(Color.red))
                            .foregroundStyle(.white)
                    }
                }
                .padding(.bottom, 40)
            }
        }
        .onReceive(timer) { _ in
            guard let startedAt else { return }
            let seconds = Int(Date().timeIntervalSince(startedAt))
            elapsedText = String(format: "%02d:%02d", seconds / 60, seconds % 60)
        }
    }
}
