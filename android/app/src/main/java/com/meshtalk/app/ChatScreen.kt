package com.meshtalk.app

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ChatBubbleOutline
import androidx.compose.material.icons.filled.Lock
import androidx.compose.material.icons.filled.Warning
import androidx.compose.material3.Divider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import uniffi.mesh_mobile.DiscoveredPeer

/**
 * Home screen for the Chat tab: one card per connected device, like a normal chat app's
 * conversation list -- not a single shared feed. Tapping a card opens that person's
 * [ChatThreadScreen].
 */
@Composable
fun ChatScreen(store: MeshStore, onOpenChat: (DiscoveredPeer) -> Unit, modifier: Modifier = Modifier) {
    val conversations = store.connectedPeers.values.sortedBy { it.displayName.lowercase() }

    Column(modifier = modifier.fillMaxSize()) {
        StatusBar(store)

        if (conversations.isEmpty()) {
            EmptyConversationsState(modifier = Modifier.fillMaxSize())
        } else {
            LazyColumn(modifier = Modifier.fillMaxSize()) {
                items(conversations, key = { it.fullNodeId }) { peer ->
                    ConversationRow(store, peer, onClick = { onOpenChat(peer) })
                    Divider()
                }
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
private fun EmptyConversationsState(modifier: Modifier = Modifier) {
    Column(
        modifier = modifier.padding(32.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center,
    ) {
        Icon(
            Icons.Filled.ChatBubbleOutline,
            contentDescription = null,
            modifier = Modifier.size(40.dp),
            tint = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Spacer(modifier = Modifier.padding(top = 8.dp))
        Text("No conversations yet", style = MaterialTheme.typography.titleMedium)
        Spacer(modifier = Modifier.padding(top = 4.dp))
        Text(
            "Connect to a nearby device in Settings to start chatting.",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun ConversationRow(store: MeshStore, peer: DiscoveredPeer, onClick: () -> Unit) {
    val online = store.isOnline(peer.fullNodeId)
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 16.dp, vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(
            modifier = Modifier
                .size(44.dp)
                .clip(CircleShape)
                .background(MaterialTheme.colorScheme.primary.copy(alpha = 0.2f)),
            contentAlignment = Alignment.Center,
        ) {
            Text(
                peer.displayName.take(1).uppercase(),
                color = MaterialTheme.colorScheme.primary,
                fontWeight = FontWeight.Bold,
            )
        }

        Spacer(modifier = Modifier.padding(start = 6.dp))

        Column(modifier = Modifier.weight(1f).padding(start = 6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(peer.displayName, fontWeight = FontWeight.Medium)
                Spacer(modifier = Modifier.padding(start = 4.dp))
                Box(
                    modifier = Modifier
                        .size(7.dp)
                        .clip(CircleShape)
                        .background(if (online) Color(0xFF34C759) else MaterialTheme.colorScheme.outline),
                )
                if (store.identityChangedPeerIds.contains(peer.fullNodeId)) {
                    Spacer(modifier = Modifier.padding(start = 4.dp))
                    Icon(
                        Icons.Filled.Warning,
                        contentDescription = "Identity changed",
                        tint = MaterialTheme.colorScheme.error,
                        modifier = Modifier.size(12.dp),
                    )
                } else if (store.isSecure(peer.fullNodeId)) {
                    Spacer(modifier = Modifier.padding(start = 4.dp))
                    Icon(
                        Icons.Filled.Lock,
                        contentDescription = "Secure",
                        tint = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.size(12.dp),
                    )
                }
            }
            Text(
                store.lastMessagePreview(peer.fullNodeId) ?: "Say hello \uD83D\uDC4B",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                maxLines = 1,
            )
        }

        Text(
            if (online) "Online" else "Offline",
            style = MaterialTheme.typography.labelSmall,
            color = if (online) Color(0xFF34C759) else MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

