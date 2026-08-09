package com.meshtalk.app

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import uniffi.mesh_mobile.DiscoveredPeer

@Composable
fun SettingsScreen(store: MeshStore, modifier: Modifier = Modifier) {
    var displayName by remember { mutableStateOf("") }
    var listenPort by remember { mutableIntStateOf(9001) }
    var channel by remember { mutableStateOf("mesh-demo") }
    var manualAddress by remember { mutableStateOf("") }

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

        store.lastError?.let {
            Text(it, color = MaterialTheme.colorScheme.error, modifier = Modifier.padding(bottom = 16.dp))
        }

        Button(
            onClick = { store.start(displayName, listenPort, channel) },
            enabled = displayName.isNotBlank(),
        ) {
            Text(if (store.isConnected) "Restart" else "Start")
        }

        if (store.isConnected) {
            Button(onClick = { store.disconnect() }, modifier = Modifier.padding(top = 8.dp)) {
                Text("Stop")
            }

            Text(
                "Nearby devices",
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.padding(top = 24.dp),
            )
            if (store.discoveredPeers.isEmpty()) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.padding(top = 8.dp),
                ) {
                    CircularProgressIndicator(modifier = Modifier.padding(end = 8.dp))
                    Text("Looking for nearby devices...", color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
            } else {
                for (peer in store.discoveredPeers) {
                    NearbyPeerRow(peer, store)
                }
            }
            Text(
                "Found automatically on your Wi-Fi network, like Bluetooth pairing -- no IP " +
                    "address needed. Compare the code shown here with the one on the other " +
                    "device before connecting.",
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(top = 4.dp, bottom = 16.dp),
            )

            Text("Advanced", style = MaterialTheme.typography.titleMedium)
            OutlinedTextField(
                value = manualAddress,
                onValueChange = { manualAddress = it },
                label = { Text("IP:port (e.g. 192.168.1.42:9001)") },
                modifier = Modifier.fillMaxWidth().padding(top = 8.dp),
            )
            Button(
                onClick = {
                    store.connectManually(manualAddress)
                    manualAddress = ""
                },
                enabled = manualAddress.isNotBlank(),
                modifier = Modifier.padding(top = 8.dp),
            ) {
                Text("Connect manually")
            }
            Text(
                "Only needed if a device isn't on the same local network as auto-discovery.",
                style = MaterialTheme.typography.bodySmall,
                modifier = Modifier.padding(top = 4.dp),
            )
        }
    }
}

@Composable
private fun NearbyPeerRow(peer: DiscoveredPeer, store: MeshStore) {
    val isConnected = store.connectedAddresses.contains(peer.address)
    Row(
        modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column {
            Text(peer.displayName)
            Text(
                "code ${peer.pairingCode}",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        if (isConnected) {
            Icon(Icons.Filled.CheckCircle, contentDescription = "Connected", tint = MaterialTheme.colorScheme.primary)
        } else {
            Button(onClick = { store.connect(peer) }) {
                Text("Connect")
            }
        }
    }
}
