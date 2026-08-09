package com.meshtalk.app

import android.Manifest
import android.graphics.BitmapFactory
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.AttachFile
import androidx.compose.material.icons.filled.Mic
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.Button
import androidx.compose.material3.IconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import uniffi.mesh_mobile.AttachmentKind
import uniffi.mesh_mobile.FileAttachment
import uniffi.mesh_mobile.ReceivedMessage

@Composable
fun ChatScreen(store: MeshStore, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    var draft by remember { mutableStateOf("") }
    val listState = rememberLazyListState()
    val voiceRecorder = remember { VoiceRecorder(context) }
    var isRecording by remember { mutableStateOf(false) }

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
                    store.sendFile(bytes, "video.mp4", mimeType, AttachmentKind.VIDEO)
                } else {
                    store.sendFile(bytes, "photo.jpg", mimeType, AttachmentKind.IMAGE)
                }
            }
        }
    }

    LaunchedEffect(store.messages.size) {
        if (store.messages.isNotEmpty()) {
            listState.animateScrollToItem(store.messages.size - 1)
        }
    }

    Column(modifier = modifier.fillMaxSize()) {
        StatusBar(store)

        LazyColumn(
            state = listState,
            modifier = Modifier.fillMaxSize().weight(1f).padding(8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(store.messages) { message -> MessageRow(message) }
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
                        store.sendFile(data, "voice.m4a", "audio/m4a", AttachmentKind.VOICE)
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
                    store.send(draft)
                    draft = ""
                },
                enabled = store.isConnected && draft.isNotBlank(),
            ) {
                Text("Send")
            }
        }
    }
}

@Composable
private fun StatusBar(store: MeshStore) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 8.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            text = if (store.isConnected) "connected -- id ${store.nodeId}" else "not connected",
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

@Composable
private fun MessageRow(message: ReceivedMessage) {
    val text = message.text
    val attachment = message.attachment
    if (text != null) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .background(MaterialTheme.colorScheme.surfaceVariant, RoundedCornerShape(8.dp))
                .padding(8.dp),
        ) {
            Text(text)
        }
    } else if (attachment != null) {
        AttachmentRow(attachment)
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
