import AVFoundation
import Foundation

/// Captures microphone audio, converts it to a fixed wire format (16kHz mono 16-bit PCM),
/// and hands off ~20ms frames via `onEncodedFrame` -- and plays back incoming frames of
/// the same format. No codec (no Opus/AAC) -- this sends raw PCM, which is simple and
/// keeps the original audio quality (no lossy re-encoding) at the cost of more bandwidth
/// than a real VoIP codec would use (about 256kbit/s each way), which is fine on a local
/// Wi-Fi network.
final class CallAudioEngine {
    private let engine = AVAudioEngine()
    private let playerNode = AVAudioPlayerNode()
    private let onEncodedFrame: (UInt32, Data) -> Void
    private var sequence: UInt32 = 0
    private var pcmAccumulator = Data()
    private var started = false

    /// 16kHz mono 16-bit PCM -- plenty for intelligible voice, and a simple fixed target
    /// format both sides can agree on without any negotiation.
    static let sampleRate: Double = 16_000
    /// 20ms per frame at the wire sample rate -- the standard VoIP frame size.
    static let frameSampleCount = 320

    private lazy var wireFormat = AVAudioFormat(
        commonFormat: .pcmFormatInt16,
        sampleRate: Self.sampleRate,
        channels: 1,
        interleaved: true
    )!

    init(onEncodedFrame: @escaping (UInt32, Data) -> Void) {
        self.onEncodedFrame = onEncodedFrame
    }

    func start() {
        guard !started else { return }
        started = true

        let session = AVAudioSession.sharedInstance()
        try? session.setCategory(.playAndRecord, mode: .voiceChat, options: [.defaultToSpeaker, .allowBluetooth])
        try? session.setActive(true)

        let input = engine.inputNode
        let inputFormat = input.outputFormat(forBus: 0)
        let target = wireFormat
        let converter = AVAudioConverter(from: inputFormat, to: target)

        input.installTap(onBus: 0, bufferSize: 1024, format: inputFormat) { [weak self] buffer, _ in
            self?.handleCapturedBuffer(buffer, converter: converter, targetFormat: target)
        }

        engine.attach(playerNode)
        engine.connect(playerNode, to: engine.mainMixerNode, format: target)
        engine.prepare()
        try? engine.start()
        playerNode.play()
    }

    private func handleCapturedBuffer(_ buffer: AVAudioPCMBuffer, converter: AVAudioConverter?, targetFormat: AVAudioFormat) {
        guard let converter else { return }
        let ratio = targetFormat.sampleRate / buffer.format.sampleRate
        let outCapacity = AVAudioFrameCount(Double(buffer.frameLength) * ratio) + 16
        guard let outBuffer = AVAudioPCMBuffer(pcmFormat: targetFormat, frameCapacity: outCapacity) else { return }

        var hasProvidedInput = false
        var conversionError: NSError?
        let status = converter.convert(to: outBuffer, error: &conversionError) { _, outStatus in
            if hasProvidedInput {
                outStatus.pointee = .noDataNow
                return nil
            }
            hasProvidedInput = true
            outStatus.pointee = .haveData
            return buffer
        }
        guard status != .error, conversionError == nil, let channelData = outBuffer.int16ChannelData else { return }

        let frameLength = Int(outBuffer.frameLength)
        guard frameLength > 0 else { return }
        let bytes = channelData[0].withMemoryRebound(to: UInt8.self, capacity: frameLength * 2) { ptr in
            Data(bytes: ptr, count: frameLength * 2)
        }
        pcmAccumulator.append(bytes)

        let frameByteSize = Self.frameSampleCount * 2
        while pcmAccumulator.count >= frameByteSize {
            let chunk = pcmAccumulator.prefix(frameByteSize)
            pcmAccumulator.removeFirst(frameByteSize)
            onEncodedFrame(sequence, Data(chunk))
            sequence &+= 1
        }
    }

    /// Schedules one incoming frame (raw PCM in the same wire format) for playback.
    func play(data: Data) {
        guard started else { return }
        let frameCount = AVAudioFrameCount(data.count / 2)
        guard frameCount > 0, let buffer = AVAudioPCMBuffer(pcmFormat: wireFormat, frameCapacity: frameCount) else { return }
        buffer.frameLength = frameCount
        data.withUnsafeBytes { raw in
            guard let src = raw.bindMemory(to: Int16.self).baseAddress, let dst = buffer.int16ChannelData?[0] else { return }
            dst.update(from: src, count: Int(frameCount))
        }
        playerNode.scheduleBuffer(buffer, completionHandler: nil)
    }

    func stop() {
        guard started else { return }
        started = false
        engine.inputNode.removeTap(onBus: 0)
        playerNode.stop()
        engine.stop()
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }
}
