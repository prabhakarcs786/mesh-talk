package com.meshtalk.app

import android.Manifest
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Call
import androidx.compose.material.icons.filled.CallEnd
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.MicOff
import androidx.compose.material.icons.filled.Videocam
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.delay

/**
 * Shown as a full-screen overlay whenever `store.callPhase != Idle` -- an incoming-call
 * banner, an outgoing "ringing" screen, or the active in-call screen (audio or video).
 * Mirrors `ios/MeshTalk/CallView.swift`.
 */
@Composable
fun CallOverlay(store: MeshStore) {
    when (val phase = store.callPhase) {
        is CallPhase.Idle -> {}
        is CallPhase.IncomingRinging -> IncomingCallScreen(store, phase)
        is CallPhase.OutgoingRinging -> InCallScreen(
            store,
            name = phase.remoteName,
            video = phase.video,
            isRinging = true,
            startedAtMs = null,
        )
        is CallPhase.Active -> InCallScreen(
            store,
            name = phase.remoteName,
            video = phase.video,
            isRinging = false,
            startedAtMs = phase.startedAtMs,
        )
    }
}

@Composable
private fun IncomingCallScreen(store: MeshStore, phase: CallPhase.IncomingRinging) {
    Box(modifier = Modifier.fillMaxSize().background(Color.Black.copy(alpha = 0.92f))) {
        Column(
            modifier = Modifier.fillMaxSize().padding(32.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Spacer(Modifier.weight(1f))
            Icon(
                if (phase.video) Icons.Filled.Videocam else Icons.Filled.Call,
                contentDescription = null,
                tint = Color.White,
                modifier = Modifier.size(56.dp),
            )
            Text(phase.remoteName, color = Color.White, fontSize = 28.sp, modifier = Modifier.padding(top = 16.dp))
            Text(
                if (phase.video) "Incoming video call..." else "Incoming call...",
                color = Color.White.copy(alpha = 0.7f),
                modifier = Modifier.padding(top = 8.dp),
            )
            Spacer(Modifier.weight(1f))
            Row(
                horizontalArrangement = Arrangement.spacedBy(60.dp),
                modifier = Modifier.padding(bottom = 60.dp),
            ) {
                CallCircleButton(icon = Icons.Filled.CallEnd, background = Color.Red) { store.rejectIncomingCall() }
                CallCircleButton(icon = Icons.Filled.Call, background = Color(0xFF34C759)) { store.acceptIncomingCall() }
            }
        }
    }
}

@Composable
private fun InCallScreen(store: MeshStore, name: String, video: Boolean, isRinging: Boolean, startedAtMs: Long?) {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    var elapsedText by remember { mutableStateOf("00:00") }
    var videoCapture by remember { mutableStateOf<CallVideoCapture?>(null) }

    val cameraPermissionLauncher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) {
            val capture = CallVideoCapture(context) { sequence, data -> store.sendActiveCallVideoFrame(sequence, data) }
            capture.start(lifecycleOwner)
            videoCapture = capture
        }
    }

    LaunchedEffect(video) {
        if (video) {
            cameraPermissionLauncher.launch(Manifest.permission.CAMERA)
        }
    }

    DisposableEffect(Unit) {
        onDispose { videoCapture?.stop() }
    }

    LaunchedEffect(startedAtMs) {
        if (startedAtMs == null) return@LaunchedEffect
        while (true) {
            val seconds = (System.currentTimeMillis() - startedAtMs) / 1000
            elapsedText = String.format("%02d:%02d", seconds / 60, seconds % 60)
            delay(1000)
        }
    }

    Box(modifier = Modifier.fillMaxSize().background(Color.Black)) {
        if (video) {
            RemoteVideoView(store.remoteVideoFrame, modifier = Modifier.fillMaxSize())
        }

        Column(modifier = Modifier.fillMaxSize().padding(24.dp)) {
            Spacer(Modifier.weight(1f))
            Text(name, color = Color.White, fontSize = 28.sp, modifier = Modifier.align(Alignment.CenterHorizontally))
            Text(
                if (isRinging) "Ringing..." else elapsedText,
                color = Color.White.copy(alpha = 0.8f),
                modifier = Modifier.align(Alignment.CenterHorizontally).padding(top = 4.dp),
            )
            Spacer(Modifier.weight(1f))

            videoCapture?.let { capture ->
                LocalVideoPreview(
                    capture = capture,
                    modifier = Modifier
                        .align(Alignment.End)
                        .width(110.dp)
                        .height(150.dp)
                        .padding(bottom = 16.dp),
                )
            }

            Row(
                horizontalArrangement = Arrangement.spacedBy(40.dp),
                modifier = Modifier.align(Alignment.CenterHorizontally).padding(bottom = 40.dp),
            ) {
                CallCircleButton(
                    icon = if (store.isMuted) Icons.Filled.MicOff else Icons.Filled.Mic,
                    background = Color.White.copy(alpha = 0.2f),
                ) { store.toggleMute() }
                CallCircleButton(icon = Icons.Filled.CallEnd, background = Color.Red) { store.hangUp() }
            }
        }
    }
}

@Composable
private fun CallCircleButton(icon: ImageVector, background: Color, onClick: () -> Unit) {
    IconButton(
        onClick = onClick,
        modifier = Modifier.size(64.dp).background(background, shape = CircleShape),
    ) {
        Icon(icon, contentDescription = null, tint = Color.White)
    }
}
