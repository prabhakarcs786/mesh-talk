package com.meshtalk.app

import android.annotation.SuppressLint
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaRecorder
import kotlin.concurrent.thread

/**
 * Captures microphone audio as raw 16kHz mono 16-bit PCM and hands off ~20ms frames via
 * [onEncodedFrame]; plays back incoming frames of the same format. No codec (no
 * Opus/AAC) -- sends raw PCM, keeping the original captured audio (no extra lossy
 * re-encoding) at the cost of more bandwidth than a real VoIP codec, fine on local Wi-Fi.
 * Mirrors `ios/MeshTalk/CallAudioEngine.swift`.
 */
class CallAudioEngine(private val onEncodedFrame: (Int, ByteArray) -> Unit) {
    companion object {
        const val SAMPLE_RATE = 16_000
        /** 20ms per frame at the wire sample rate -- the standard VoIP frame size. */
        const val FRAME_SAMPLE_COUNT = 320
        private const val FRAME_BYTES = FRAME_SAMPLE_COUNT * 2
    }

    private var audioRecord: AudioRecord? = null
    private var audioTrack: AudioTrack? = null
    private var captureThread: Thread? = null

    @Volatile private var running = false
    private var sequence = 0

    /**
     * Starts capture/playback. Best-effort: if `RECORD_AUDIO` isn't granted (the
     * permission is requested by the call UI, but this can be invoked from the view
     * model slightly before that completes), or if the device's audio HAL simply can't
     * open the requested format/buffer size (seen in practice on some emulator images
     * and some real devices -- `AudioRecord`/`AudioTrack` can be constructed without
     * throwing yet left in `STATE_UNINITIALIZED`, and calling `startRecording()`/
     * `play()` on that throws `IllegalStateException`, uncaught, which used to crash
     * the whole call/app -- this is the "app has a bug, try clearing cache" crash
     * reported when accepting an incoming call), this logs and leaves the engine
     * inactive rather than crashing the call -- the caller can still see/hear the
     * other side's video and won't lose the whole call over an audio-only failure.
     */
    @SuppressLint("MissingPermission")
    fun start() {
        if (running) return
        running = true

        try {
            val minRecordBuf = AudioRecord.getMinBufferSize(
                SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT,
            )
            val record = AudioRecord(
                MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT,
                maxOf(minRecordBuf, FRAME_BYTES * 4),
            )
            if (record.state != AudioRecord.STATE_INITIALIZED) {
                record.release()
                running = false
                return
            }
            audioRecord = record

            val minTrackBuf = AudioTrack.getMinBufferSize(
                SAMPLE_RATE,
                AudioFormat.CHANNEL_OUT_MONO,
                AudioFormat.ENCODING_PCM_16BIT,
            )
            val track = AudioTrack(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build(),
                AudioFormat.Builder()
                    .setSampleRate(SAMPLE_RATE)
                    .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                    .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                    .build(),
                maxOf(minTrackBuf, FRAME_BYTES * 4),
                AudioTrack.MODE_STREAM,
                AudioManager.AUDIO_SESSION_ID_GENERATE,
            )
            if (track.state != AudioTrack.STATE_INITIALIZED) {
                track.release()
                record.release()
                audioRecord = null
                running = false
                return
            }
            audioTrack = track

            record.startRecording()
            track.play()
        } catch (e: Exception) {
            // Covers SecurityException (permission not yet granted), IllegalArgumentException
            // (unsupported format/buffer size on this device's audio HAL), and
            // IllegalStateException (startRecording()/play() called on a device that failed
            // to fully initialize despite not throwing from its constructor) -- any of these
            // must degrade to "no audio this call", never crash it.
            try { audioRecord?.release() } catch (_: Exception) {}
            try { audioTrack?.release() } catch (_: Exception) {}
            audioRecord = null
            audioTrack = null
            running = false
            return
        }

        captureThread = thread(name = "meshtalk-call-audio-capture") {
            val buffer = ByteArray(FRAME_BYTES)
            while (running) {
                val record = audioRecord ?: break
                val read = try {
                    record.read(buffer, 0, buffer.size)
                } catch (_: Exception) {
                    break
                }
                if (read == buffer.size) {
                    onEncodedFrame(sequence, buffer.copyOf())
                    sequence++
                }
            }
        }
    }

    /** Schedules one incoming frame (raw PCM in the same wire format) for playback. */
    fun play(data: ByteArray) {
        try {
            audioTrack?.write(data, 0, data.size)
        } catch (_: Exception) {
        }
    }

    fun stop() {
        if (!running) return
        running = false
        captureThread?.join(500)
        captureThread = null
        try { audioRecord?.stop() } catch (_: Exception) {}
        try { audioRecord?.release() } catch (_: Exception) {}
        audioRecord = null
        try { audioTrack?.stop() } catch (_: Exception) {}
        try { audioTrack?.release() } catch (_: Exception) {}
        audioTrack = null
    }
}
