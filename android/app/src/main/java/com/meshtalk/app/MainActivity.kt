package com.meshtalk.app

import android.Manifest
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Chat
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import uniffi.mesh_mobile.DiscoveredPeer

class MainActivity : ComponentActivity() {
    private val store: MeshStore by viewModels()

    // Android 13+ requires this to be granted at runtime for MeshForegroundService's
    // persistent notification to actually show -- the service still gets foreground
    // process priority (the whole point of it -- see its doc comment) even if this is
    // denied, so this is purely about the visible "meshtalk is running" cue, not a
    // hard requirement for the mesh node to keep working in the background.
    private val notificationPermissionLauncher =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) { /* no-op either way */ }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            notificationPermissionLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
        }
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier) {
                    MeshTalkApp(store)
                }
            }
        }
    }
}

@androidx.compose.runtime.Composable
private fun MeshTalkApp(store: MeshStore) {
    var selectedTab by remember { mutableIntStateOf(0) }
    var openChatPeer by remember { mutableStateOf<DiscoveredPeer?>(null) }

    Box {
        Scaffold(
            bottomBar = {
                NavigationBar {
                    NavigationBarItem(
                        selected = selectedTab == 0,
                        onClick = { selectedTab = 0 },
                        icon = { Icon(Icons.Filled.Chat, contentDescription = "Chat") },
                        label = { Text("Chat") },
                    )
                    NavigationBarItem(
                        selected = selectedTab == 1,
                        onClick = { selectedTab = 1 },
                        icon = { Icon(Icons.Filled.Settings, contentDescription = "Settings") },
                        label = { Text("Settings") },
                    )
                }
            },
        ) { padding ->
            when (selectedTab) {
                0 -> {
                    val peer = openChatPeer
                    if (peer == null) {
                        ChatScreen(store, onOpenChat = { openChatPeer = it }, modifier = Modifier.padding(padding))
                    } else {
                        ChatThreadScreen(store, peer, onBack = { openChatPeer = null }, modifier = Modifier.padding(padding))
                    }
                }
                else -> SettingsScreen(
                    store,
                    onOpenChat = { peer ->
                        selectedTab = 0
                        openChatPeer = peer
                    },
                    modifier = Modifier.padding(padding),
                )
            }
        }

        if (store.callPhase != CallPhase.Idle) {
            CallOverlay(store)
        }
    }
}
