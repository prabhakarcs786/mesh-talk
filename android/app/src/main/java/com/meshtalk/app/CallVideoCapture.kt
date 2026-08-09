package com.meshtalk.app

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.ImageFormat
import android.graphics.Rect
import android.graphics.YuvImage
import androidx.camera.core.CameraSelector
import androidx.camera.core.ImageAnalysis
import androidx.camera.core.ImageProxy
import androidx.camera.core.Preview
import androidx.camera.lifecycle.ProcessCameraProvider
import androidx.camera.view.PreviewView
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.content.ContextCompat
import androidx.lifecycle.LifecycleOwner
import java.io.ByteArrayOutputStream
import java.util.concurrent.Executors

/**
 * Captures the front camera at a low resolution/frame rate and hands off each frame as a
 * JPEG via [onEncodedFrame] -- motion-JPEG rather than a real video codec (H.264): no
 * hardware encoder bridge needed, each frame is independently decodable, and it plays
 * nicely with a lossy, no-retransmission mesh (a dropped frame is just a skipped frame).
 * Mirrors `ios/MeshTalk/CallVideoCapture.swift`.
 */
class CallVideoCapture(private val context: Context, private val onEncodedFrame: (Int, ByteArray) -> Unit) {
    private var cameraProvider: ProcessCameraProvider? = null
    private var sequence = 0
    private var lastSendTime = 0L

    /** Throttle to ~8fps -- keeps bandwidth and CPU reasonable over a relay-based mesh. */
    private val minFrameIntervalMs = 1000L / 8
    private val analysisExecutor = Executors.newSingleThreadExecutor()

    /** Set once the camera is bound; [previewView] renders it for a local self-view. */
    var previewView: PreviewView? = null
        private set

    fun start(lifecycleOwner: LifecycleOwner) {
        val view = PreviewView(context)
        previewView = view

        val future = ProcessCameraProvider.getInstance(context)
        future.addListener({
            val provider = future.get()
            cameraProvider = provider

            val preview = Preview.Builder().build().also {
                it.setSurfaceProvider(view.surfaceProvider)
            }
            val analysis = ImageAnalysis.Builder()
                .setTargetResolution(android.util.Size(320, 240))
                .setBackpressureStrategy(ImageAnalysis.STRATEGY_KEEP_ONLY_LATEST)
                .build()
            analysis.setAnalyzer(analysisExecutor) { image -> handleFrame(image) }

            try {
                provider.unbindAll()
                provider.bindToLifecycle(lifecycleOwner, CameraSelector.DEFAULT_FRONT_CAMERA, preview, analysis)
            } catch (_: Exception) {
                // No front camera, or binding failed -- video call proceeds audio-only
                // from this side; the other side simply won't receive video frames.
            }
        }, ContextCompat.getMainExecutor(context))
    }

    private fun handleFrame(image: ImageProxy) {
        val now = System.currentTimeMillis()
        if (now - lastSendTime < minFrameIntervalMs) {
            image.close()
            return
        }
        try {
            val jpeg = image.toJpegBytes(quality = 50)
            lastSendTime = now
            onEncodedFrame(sequence, jpeg)
            sequence++
        } catch (_: Exception) {
        } finally {
            image.close()
        }
    }

    fun stop() {
        cameraProvider?.unbindAll()
        cameraProvider = null
        previewView = null
    }
}

/** Converts a YUV_420_888 `ImageAnalysis` frame to JPEG bytes. */
private fun ImageProxy.toJpegBytes(quality: Int): ByteArray {
    val yBuffer = planes[0].buffer
    val uBuffer = planes[1].buffer
    val vBuffer = planes[2].buffer

    val ySize = yBuffer.remaining()
    val uSize = uBuffer.remaining()
    val vSize = vBuffer.remaining()

    val nv21 = ByteArray(ySize + uSize + vSize)
    yBuffer.get(nv21, 0, ySize)
    vBuffer.get(nv21, ySize, vSize)
    uBuffer.get(nv21, ySize + vSize, uSize)

    val yuvImage = YuvImage(nv21, ImageFormat.NV21, width, height, null)
    val out = ByteArrayOutputStream()
    yuvImage.compressToJpeg(Rect(0, 0, width, height), quality, out)
    return out.toByteArray()
}

/** Shows the local camera preview while a video call is active. */
@Composable
fun LocalVideoPreview(capture: CallVideoCapture, modifier: Modifier = Modifier) {
    val view = capture.previewView
    if (view != null) {
        AndroidView(factory = { view }, modifier = modifier)
    }
}

/** Shows the latest received remote video frame (a JPEG `ByteArray`). */
@Composable
fun RemoteVideoView(frameData: ByteArray?, modifier: Modifier = Modifier) {
    val bitmap: Bitmap? = frameData?.let { BitmapFactory.decodeByteArray(it, 0, it.size) }
    if (bitmap != null) {
        Image(bitmap = bitmap.asImageBitmap(), contentDescription = null, modifier = modifier)
    } else {
        androidx.compose.foundation.layout.Box(modifier = modifier.background(Color.Black))
    }
}
