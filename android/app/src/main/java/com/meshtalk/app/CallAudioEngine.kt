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
     * model slightly before that completes), this just logs and leaves the engine
     * inactive rather than crashing the call -- the caller can still see/hear the other
     * side's video and won't lose the whole call over a timing race on permissions.
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
            audioRecord = AudioRecord(
                MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT,
                maxOf(minRecordBuf, FRAME_BYTES * 4),
            )
        } catch (e: SecurityException) {
            running = false
            return
        }

        val minTrackBuf = AudioTrack.getMinBufferSize(
            SAMPLE_RATE,
            AudioFormat.CHANNEL_OUT_MONO,
            AudioFormat.ENCODING_PCM_16BIT,
        )
        audioTrack = AudioTrack(
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

        audioRecord?.startRecording()
        audioTrack?.play()

        captureThread = thread(name = "meshtalk-call-audio-capture") {
            val buffer = ByteArray(FRAME_BYTES)
            while (running) {
                val record = audioRecord ?: break
                val read = record.read(buffer, 0, buffer.size)
                if (read == buffer.size) {
                    onEncodedFrame(sequence, buffer.copyOf())
                    sequence++
                }
            }
        }
    }

    /** Schedules one incoming frame (raw PCM in the same wire format) for playback. */
    fun play(data: ByteArray) {
        audioTrack?.write(data, 0, data.size)
    }

    fun stop() {
        if (!running) return
        running = false
        captureThread?.join(500)
        captureThread = null
        audioRecord?.stop()
        audioRecord?.release()
        audioRecord = null
        audioTrack?.stop()
        audioTrack?.release()
        audioTrack = null
    }
}
