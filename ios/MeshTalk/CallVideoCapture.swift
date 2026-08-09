import AVFoundation
import SwiftUI
import UIKit

/// Captures the front camera at a low resolution/frame rate and hands off each frame as a
/// JPEG via `onEncodedFrame` -- deliberately simple (motion-JPEG rather than a real video
/// codec like H.264) so it doesn't need a hardware encoder bridge; each frame is a
/// complete, independently-decodable image at its original captured quality (no
/// additional lossy re-encoding beyond the one JPEG encode), which also plays nicely with
/// a lossy, no-retransmission mesh -- a dropped video frame is just a skipped frame, not
/// corruption in a longer-lived encoder state (as a dropped H.264 P-frame would cause).
final class CallVideoCapture: NSObject, ObservableObject {
    private let session = AVCaptureSession()
    private let onEncodedFrame: (UInt32, Data) -> Void
    private var sequence: UInt32 = 0
    private var lastSendTime: CFAbsoluteTime = 0
    /// Throttle to ~8fps -- keeps bandwidth and CPU reasonable over a relay-based mesh.
    private let minFrameInterval: CFAbsoluteTime = 1.0 / 8.0
    private let queue = DispatchQueue(label: "meshtalk.call-video-capture")
    private var started = false

    /// `false` until capture has been attempted and no usable camera was found (e.g. the
    /// iOS Simulator, which has no real camera hardware) -- lets the UI show a clear
    /// "camera not available" message instead of a plain black screen that could be
    /// mistaken for a crash or a stuck connection.
    @Published private(set) var cameraAvailable = true

    init(onEncodedFrame: @escaping (UInt32, Data) -> Void) {
        self.onEncodedFrame = onEncodedFrame
    }

    /// A preview layer for showing the local camera feed, if the caller wants one.
    func makePreviewLayer() -> AVCaptureVideoPreviewLayer {
        let layer = AVCaptureVideoPreviewLayer(session: session)
        layer.videoGravity = .resizeAspectFill
        return layer
    }

    func start() {
        guard !started else { return }
        started = true
        queue.async { [weak self] in
            self?.configureAndStart()
        }
    }

    private func configureAndStart() {
        session.beginConfiguration()
        session.sessionPreset = .vga640x480

        let haveCamera: Bool
        if let camera = AVCaptureDevice.default(.builtInWideAngleCamera, for: .video, position: .front),
           let input = try? AVCaptureDeviceInput(device: camera), session.canAddInput(input) {
            session.addInput(input)
            haveCamera = true
        } else {
            // No usable front camera -- notably always the case in the iOS Simulator,
            // which doesn't support camera capture at all. The call still proceeds
            // audio-only from this side; the UI reflects this via `cameraAvailable`
            // instead of silently showing a black rectangle.
            haveCamera = false
        }
        DispatchQueue.main.async { [weak self] in
            self?.cameraAvailable = haveCamera
        }

        let output = AVCaptureVideoDataOutput()
        output.videoSettings = [kCVPixelBufferPixelFormatTypeKey as String: kCVPixelFormatType_32BGRA]
        output.alwaysDiscardsLateVideoFrames = true
        output.setSampleBufferDelegate(self, queue: queue)
        if session.canAddOutput(output) {
            session.addOutput(output)
        }

        session.commitConfiguration()
        if haveCamera {
            session.startRunning()
        }
    }

    func stop() {
        guard started else { return }
        started = false
        queue.async { [weak self] in
            self?.session.stopRunning()
        }
    }
}

extension CallVideoCapture: AVCaptureVideoDataOutputSampleBufferDelegate {
    func captureOutput(_ output: AVCaptureOutput, didOutput sampleBuffer: CMSampleBuffer, from connection: AVCaptureConnection) {
        let now = CFAbsoluteTimeGetCurrent()
        guard now - lastSendTime >= minFrameInterval else { return }

        guard let pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        let ciImage = CIImage(cvPixelBuffer: pixelBuffer)
        let context = CIContext()
        guard let cgImage = context.createCGImage(ciImage, from: ciImage.extent) else { return }
        let uiImage = UIImage(cgImage: cgImage)
        guard let jpeg = uiImage.jpegData(compressionQuality: 0.5) else { return }

        lastSendTime = now
        onEncodedFrame(sequence, jpeg)
        sequence &+= 1
    }
}

/// Displays the local camera preview (via `CallVideoCapture`) in SwiftUI, or a clear
/// placeholder if no camera is available (e.g. the iOS Simulator).
struct LocalVideoPreview: View {
    @ObservedObject var capture: CallVideoCapture

    var body: some View {
        if capture.cameraAvailable {
            CameraPreviewLayerView(capture: capture)
        } else {
            noCameraPlaceholder
        }
    }

    private var noCameraPlaceholder: some View {
        ZStack {
            Color.black
            VStack(spacing: 4) {
                Image(systemName: "video.slash.fill")
                    .foregroundStyle(.white)
                Text("No camera")
                    .font(.caption2)
                    .foregroundStyle(.white)
            }
        }
    }
}

private struct CameraPreviewLayerView: UIViewRepresentable {
    let capture: CallVideoCapture

    func makeUIView(context: Context) -> UIView {
        let view = UIView()
        let layer = capture.makePreviewLayer()
        layer.frame = view.bounds
        view.layer.addSublayer(layer)
        context.coordinator.previewLayer = layer
        return view
    }

    func updateUIView(_ uiView: UIView, context: Context) {
        context.coordinator.previewLayer?.frame = uiView.bounds
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        var previewLayer: AVCaptureVideoPreviewLayer?
    }
}

/// Displays the latest received remote video frame (a JPEG `Data`), or a clear
/// placeholder while none has arrived yet (rather than an ambiguous plain black screen).
struct RemoteVideoView: View {
    let frameData: Data?

    var body: some View {
        if let frameData, let uiImage = UIImage(data: frameData) {
            Image(uiImage: uiImage)
                .resizable()
                .scaledToFit()
        } else {
            ZStack {
                Color.black
                VStack(spacing: 8) {
                    ProgressView()
                        .tint(.white)
                    Text("Waiting for video...")
                        .font(.caption)
                        .foregroundStyle(.white.opacity(0.8))
                }
            }
        }
    }
}
