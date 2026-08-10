package com.meshtalk.app

import android.Manifest
import android.graphics.BitmapFactory
import androidx.activity.compose.BackHandler
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ArrowBack
import androidx.compose.material.icons.filled.AttachFile
import androidx.compose.material.icons.filled.Call
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.LockOpen
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material.icons.filled.Videocam
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import uniffi.mesh_mobile.AttachmentKind
import uniffi.mesh_mobile.DiscoveredPeer
import uniffi.mesh_mobile.FileAttachment
import uniffi.mesh_mobile.ReceivedMessage
import uniffi.mesh_mobile.TransferDirection
import uniffi.mesh_mobile.TransferProgressUpdate

/**
 * One conversation with a single peer -- like opening a contact's thread in a normal
 * chat app. Shows just the messages exchanged with [peer], with call buttons for that
 * same peer right in the top bar.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatThreadScreen(store: MeshStore, peer: DiscoveredPeer, onBack: () -> Unit, modifier: Modifier = Modifier) {
    BackHandler(onBack = onBack)

    val context = LocalContext.current
    var draft by remember { mutableStateOf("") }
    val listState = rememberLazyListState()
    val voiceRecorder = remember { VoiceRecorder(context) }
    var isRecording by remember { mutableStateOf(false) }

    val threadMessages = store.messages.filter { it.peerId == peer.fullNodeId }

    val micPermissionLauncher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) {
            voiceRecorder.startRecording()
            isRecording = true
        }
    }

    val mediaPickerLauncher = rememberLauncherForActivityResult(ActivityResultContracts.PickVisualMedia()) { uri ->
        if (uri != null) {
            val bytes = context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
            if (bytes != null) {
                val mimeType = context.contentResolver.getType(uri) ?: "application/octet-stream"
                val isVideo = mimeType.startsWith("video")
                if (isVideo) {
                    store.sendFile(bytes, "video.mp4", mimeType, AttachmentKind.VIDEO, peer.fullNodeId)
                } else {
                    store.sendFile(bytes, "photo.jpg", mimeType, AttachmentKind.IMAGE, peer.fullNodeId)
                }
            }
        }
    }

    LaunchedEffect(threadMessages.size) {
        if (threadMessages.isNotEmpty()) {
            listState.animateScrollToItem(threadMessages.size - 1)
        }
    }
    LaunchedEffect(store.activeTransferOrder.size) {
        val totalItems = threadMessages.size + store.activeTransferOrder.size
        if (totalItems > 0) {
            listState.animateScrollToItem(totalItems - 1)
        }
    }

    Column(modifier = modifier.fillMaxSize()) {
        TopAppBar(
            navigationIcon = {
                IconButton(onClick = onBack) {
                    Icon(Icons.Filled.ArrowBack, contentDescription = "Back")
                }
            },
            title = {
                Column {
                    Text(peer.displayName)
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        val online = store.isOnline(peer.fullNodeId)
                        Box(
                            modifier = Modifier
                                .size(6.dp)
                                .clip(CircleShape)
                                .background(if (online) Color(0xFF34C759) else MaterialTheme.colorScheme.outline),
                        )
                        Spacer(modifier = Modifier.padding(start = 4.dp))
                        Text(
                            if (online) "Online" else "Offline",
                            style = MaterialTheme.typography.labelSmall,
                        )
                        Spacer(modifier = Modifier.padding(start = 4.dp))
                        SecurityBadge(store = store, peer = peer)
                    }
                }
            },
            actions = {
                IconButton(
                    onClick = { store.placeCall(peer, video = false) },
                    enabled = store.callPhase == CallPhase.Idle,
                ) {
                    Icon(Icons.Filled.Call, contentDescription = "Voice call")
                }
                IconButton(
                    onClick = { store.placeCall(peer, video = true) },
                    enabled = store.callPhase == CallPhase.Idle,
                ) {
                    Icon(Icons.Filled.Videocam, contentDescription = "Video call")
                }
            },
        )

        LazyColumn(
            state = listState,
            modifier = Modifier.fillMaxSize().weight(1f).padding(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(threadMessages) { message -> MessageRow(message) }
            items(store.activeTransferOrder) { transferId ->
                store.activeTransfers[transferId]?.let { TransferProgressRow(it) }
            }
        }

        Row(
            modifier = Modifier.fillMaxWidth().padding(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            IconButton(onClick = {
                mediaPickerLauncher.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageAndVideo))
            }) {
                Icon(Icons.Filled.AttachFile, contentDescription = "Attach photo or video")
            }

            IconButton(onClick = {
                if (isRecording) {
                    val data = voiceRecorder.stopRecording()
                    isRecording = false
                    if (data != null) {
                        store.sendFile(data, "voice.m4a", "audio/m4a", AttachmentKind.VOICE, peer.fullNodeId)
                    }
                } else {
                    micPermissionLauncher.launch(Manifest.permission.RECORD_AUDIO)
                }
            }) {
                Icon(
                    if (isRecording) Icons.Filled.Stop else Icons.Filled.Mic,
                    contentDescription = "Record voice note",
                    tint = if (isRecording) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurface,
                )
            }

            OutlinedTextField(
                value = draft,
                onValueChange = { draft = it },
                modifier = Modifier.weight(1f),
                placeholder = { Text("Message") },
            )
            Button(
                onClick = {
                    store.send(draft, peer.fullNodeId)
                    draft = ""
                },
                enabled = store.isConnected && draft.isNotBlank(),
            ) {
                Text("Send")
            }
        }
    }

    if (store.identityChangedPeerIds.contains(peer.fullNodeId)) {
        AlertDialog(
            onDismissRequest = { store.acknowledgeIdentityChange(peer.fullNodeId) },
            title = { Text("Security identity changed") },
            text = {
                Text(
                    "${peer.displayName}'s secure identity changed since you last talked. " +
                        "This could mean they reinstalled the app -- or it could mean someone " +
                        "else is impersonating them. Verify their identity again before " +
                        "trusting new messages.",
                )
            },
            confirmButton = {
                TextButton(onClick = { store.acknowledgeIdentityChange(peer.fullNodeId) }) {
                    Text("OK")
                }
            },
        )
    }
}

/**
 * "MeshTalk Direct Encryption v1" is real, per-recipient authenticated encryption once
 * this device holds the peer's cryptographic identity -- but that's not the same as
 * *human* identity verification (a later QR/safety-number milestone), so this
 * deliberately says "Secure" (key ownership proven), not "Verified".
 */
@Composable
private fun SecurityBadge(store: MeshStore, peer: DiscoveredPeer) {
    when {
        store.identityChangedPeerIds.contains(peer.fullNodeId) -> {
            Icon(
                Icons.Filled.Warning,
                contentDescription = "Identity changed",
                tint = MaterialTheme.colorScheme.error,
                modifier = Modifier.size(14.dp),
            )
        }
        store.isSecure(peer.fullNodeId) -> {
            Icon(
                Icons.Filled.Lock,
                contentDescription = "Secure",
                tint = Color(0xFF34C759),
                modifier = Modifier.size(14.dp),
            )
        }
        else -> {
            Icon(
                Icons.Filled.LockOpen,
                contentDescription = "Secure identity unavailable",
                tint = MaterialTheme.colorScheme.outline,
                modifier = Modifier.size(14.dp),
            )
        }
    }
}

@Composable
private fun MessageRow(message: ReceivedMessage) {
    val text = message.text
    val attachment = message.attachment
    val isMine = message.senderId == OWN_MESSAGE_SENDER_ID
    Row(modifier = Modifier.fillMaxWidth()) {
        if (isMine) Spacer(modifier = Modifier.weight(1f, fill = false).widthIn(min = 40.dp))
        Box(
            modifier = Modifier
                .weight(1f, fill = false)
                .background(
                    if (isMine) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.surfaceVariant,
                    RoundedCornerShape(8.dp),
                )
                .padding(8.dp),
        ) {
            if (text != null) {
                Text(text, color = if (isMine) MaterialTheme.colorScheme.onPrimary else MaterialTheme.colorScheme.onSurface)
            } else if (attachment != null) {
                AttachmentRow(attachment)
            }
        }
        if (!isMine) Spacer(modifier = Modifier.weight(1f, fill = false).widthIn(min = 40.dp))
    }
}

@Composable
private fun AttachmentRow(attachment: FileAttachment) {
    when (attachment.kind) {
        AttachmentKind.IMAGE -> {
            val bitmap = remember(attachment.data) {
                BitmapFactory.decodeByteArray(attachment.data, 0, attachment.data.size)
            }
            if (bitmap != null) {
                Image(
                    bitmap = bitmap.asImageBitmap(),
                    contentDescription = attachment.name,
                    modifier = Modifier.size(220.dp),
                )
            } else {
                FilePlaceholder(attachment)
            }
        }
        AttachmentKind.VIDEO -> VideoAttachmentView(attachment.data)
        AttachmentKind.VOICE -> VoicePlaybackButton(attachment.data)
        AttachmentKind.FILE -> FilePlaceholder(attachment)
    }
}

@Composable
private fun FilePlaceholder(attachment: FileAttachment) {
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surfaceVariant, RoundedCornerShape(8.dp))
            .padding(8.dp),
    ) {
        Text(attachment.name)
    }
}

@Composable
private fun TransferProgressRow(progress: TransferProgressUpdate) {
    val fraction = if (progress.totalChunks > 0u) progress.doneChunks.toFloat() / progress.totalChunks.toFloat() else 0f
    val verb = if (progress.direction == TransferDirection.SENDING) "Sending" else "Receiving"
    val kindLabel = when (progress.kind) {
        AttachmentKind.IMAGE -> "photo"
        AttachmentKind.VIDEO -> "video"
        AttachmentKind.VOICE -> "voice note"
        AttachmentKind.FILE -> "file"
    }
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.6f), RoundedCornerShape(8.dp))
            .padding(8.dp),
    ) {
        Text(
            text = "$verb $kindLabel... ${(fraction * 100).toInt()}%",
            style = MaterialTheme.typography.bodySmall,
        )
        LinearProgressIndicator(
            progress = { fraction },
            modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
        )
    }
}
