package com.meshtalk.app

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp

@Composable
fun SettingsScreen(store: MeshStore, modifier: Modifier = Modifier) {
    var displayName by remember { mutableStateOf("") }
    var listenPort by remember { mutableIntStateOf(9001) }
    var channel by remember { mutableStateOf("mesh-demo") }
    var peerAddrsText by remember { mutableStateOf("") }

    Column(modifier = modifier.fillMaxWidth().padding(16.dp)) {
        Text("Identity", style = MaterialTheme.typography.titleMedium)
        OutlinedTextField(
            value = displayName,
            onValueChange = { displayName = it },
            label = { Text("Display name") },
            modifier = Modifier.fillMaxWidth().padding(top = 8.dp, bottom = 16.dp),
        )

        Text("Network", style = MaterialTheme.typography.titleMedium)
        OutlinedTextField(
            value = listenPort.toString(),
            onValueChange = { listenPort = it.toIntOrNull() ?: listenPort },
            label = { Text("Listen port") },
            modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
        )
        OutlinedTextField(
            value = channel,
            onValueChange = { channel = it },
            label = { Text("Channel passphrase") },
            modifier = Modifier.fillMaxWidth().padding(top = 8.dp, bottom = 16.dp),
        )

        Text("Peers", style = MaterialTheme.typography.titleMedium)
        OutlinedTextField(
            value = peerAddrsText,
            onValueChange = { peerAddrsText = it },
            label = { Text("Peer addresses (comma-separated, e.g. 192.168.1.42:9001)") },
            modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
        )
        Text(
            "Today's transport is Wi-Fi/UDP-based, so list the IP:port of devices on the " +
                "same network you want to relay with directly. Bluetooth LE auto-discovery " +
                "(no manual addresses needed) is on the roadmap -- see the repo issue tracker.",
            style = MaterialTheme.typography.bodySmall,
            modifier = Modifier.padding(top = 4.dp, bottom = 16.dp),
        )

        store.lastError?.let {
            Text(it, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(bottom = 16.dp))
        }

        Button(
            onClick = {
                val peers = peerAddrsText.split(",").map { it.trim() }.filter { it.isNotEmpty() }
                store.connect(displayName, listenPort, peers, channel)
            },
            enabled = displayName.isNotBlank(),
        ) {
            Text(if (store.isConnected) "Reconnect" else "Connect")
        }

        if (store.isConnected) {
            Button(onClick = { store.disconnect() }, modifier = Modifier.padding(top = 8.dp)) {
                Text("Disconnect")
            }
        }
    }
}
