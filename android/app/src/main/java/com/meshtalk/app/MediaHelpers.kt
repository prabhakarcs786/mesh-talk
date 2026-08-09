package com.meshtalk.app

import android.content.Context
import android.media.MediaPlayer
import android.media.MediaRecorder
import android.net.Uri
import android.widget.MediaController
import android.widget.VideoView
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.Button
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import java.io.File
import java.io.FileOutputStream

/** Records short voice notes to a temporary AAC file and hands back the raw bytes. */
class VoiceRecorder(private val context: Context) {
    private var recorder: MediaRecorder? = null
    private var file: File? = null

    fun startRecording() {
        val outFile = File.createTempFile("voice", ".m4a", context.cacheDir)
        file = outFile

        @Suppress("DEPRECATION")
        val mediaRecorder = if (android.os.Build.VERSION.SDK_INT >= 31) {
            MediaRecorder(context)
        } else {
            MediaRecorder()
        }
        mediaRecorder.apply {
            setAudioSource(MediaRecorder.AudioSource.MIC)
            setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
            setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
            setAudioSamplingRate(12_000)
            setOutputFile(outFile.absolutePath)
            prepare()
            start()
        }
        recorder = mediaRecorder
    }

    /** Stops recording and returns the recorded audio bytes, or `null` if nothing usable. */
    fun stopRecording(): ByteArray? {
        return try {
            recorder?.stop()
            recorder?.release()
            recorder = null
            file?.readBytes()
        } catch (e: Exception) {
            null
        } finally {
            file?.delete()
            file = null
        }
    }
}

/** Play/pause button for a received voice note. */
@Composable
fun VoicePlaybackButton(data: ByteArray) {
    val context = LocalContext.current
    var isPlaying by remember { mutableStateOf(false) }
    var player by remember { mutableStateOf<MediaPlayer?>(null) }

    Button(onClick = {
        if (isPlaying) {
            player?.stop()
            player?.release()
            player = null
            isPlaying = false
        } else {
            val tempFile = File.createTempFile("voice_playback", ".m4a", context.cacheDir)
            FileOutputStream(tempFile).use { it.write(data) }
            val mediaPlayer = MediaPlayer()
            mediaPlayer.setDataSource(tempFile.absolutePath)
            mediaPlayer.prepare()
            mediaPlayer.setOnCompletionListener {
                isPlaying = false
                tempFile.delete()
            }
            mediaPlayer.start()
            player = mediaPlayer
            isPlaying = true
        }
    }) {
        Icon(if (isPlaying) Icons.Filled.Stop else Icons.Filled.PlayArrow, contentDescription = "Play voice note")
        Text(" Voice note", modifier = Modifier.padding(start = 4.dp))
    }
}

/** Inline video player for a received video attachment. */
@Composable
fun VideoAttachmentView(data: ByteArray) {
    val context = LocalContext.current
    val videoFile = remember(data) {
        val f = File.createTempFile("video", ".mp4", context.cacheDir)
        FileOutputStream(f).use { it.write(data) }
        f
    }

    AndroidView(
        modifier = Modifier
            .padding(4.dp)
            .background(MaterialTheme.colorScheme.surfaceVariant, RoundedCornerShape(8.dp)),
        factory = {
            VideoView(context).apply {
                setVideoURI(Uri.fromFile(videoFile))
                setMediaController(MediaController(context).also { it.setAnchorView(this) })
                setOnPreparedListener { it.isLooping = false }
            }
        },
    )
}
