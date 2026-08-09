package com.meshtalk.app

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.mesh_mobile.DiscoveredPeer
import uniffi.mesh_mobile.MeshClient
import uniffi.mesh_mobile.ReceivedMessage

/**
 * Thin observable wrapper around the Rust `MeshClient`, polling for incoming messages on
 * a coroutine loop since UniFFI's `pollMessage()` is a plain non-blocking call, not a
 * stream/callback. Mirrors `ios/MeshTalk/MeshStore.swift`.
 */
class MeshStore : ViewModel() {
    var isConnected by mutableStateOf(false)
        private set
    var nodeId by mutableStateOf("")
        private set
    var lastError by mutableStateOf<String?>(null)
        private set
    val messages = mutableStateListOf<ReceivedMessage>()

    /** Nearby devices found automatically on the Wi-Fi network -- no IP typing needed. */
    var discoveredPeers by mutableStateOf<List<DiscoveredPeer>>(emptyList())
        private set
    val connectedAddresses = mutableStateListOf<String>()

    private var client: MeshClient? = null
    private var pollJob: Job? = null
    private var discoveryJob: Job? = null

    /**
     * Starts a mesh node and immediately starts LAN auto-discovery, so nearby devices
     * running meshtalk on the same Wi-Fi network show up on their own -- connecting to
     * one is then a single tap ([connect]) instead of typing an IP address.
     */
    fun start(displayName: String, listenPort: Int, channel: String) {
        disconnect()
        try {
            val newClient = MeshClient(
                displayName = displayName,
                listenAddr = "0.0.0.0:$listenPort",
                peerAddrs = emptyList(),
                channelPassphrase = channel,
                ttl = 16u,
            )
            newClient.startDiscovery()
            client = newClient
            nodeId = newClient.nodeId()
            isConnected = true
            lastError = null
            startPolling()
            startDiscoveryPolling()
        } catch (e: Exception) {
            lastError = e.message ?: e.toString()
            isConnected = false
        }
    }

    /** One-tap connect to a device found via auto-discovery -- no manual IP entry. */
    fun connect(peer: DiscoveredPeer) {
        client?.addPeer(peer.address)
        connectedAddresses.add(peer.address)
    }

    /** Fallback for when auto-discovery doesn't find a peer (e.g. different subnet). */
    fun connectManually(address: String) {
        if (address.isBlank()) return
        client?.addPeer(address)
        connectedAddresses.add(address)
    }

    fun disconnect() {
        pollJob?.cancel()
        pollJob = null
        discoveryJob?.cancel()
        discoveryJob = null
        client = null
        isConnected = false
        discoveredPeers = emptyList()
        connectedAddresses.clear()
    }

    fun send(text: String) {
        val current = client ?: return
        if (text.isBlank()) return
        if (!current.send(text)) {
            lastError = "Failed to send -- no reachable peers right now."
        }
    }

    private fun startPolling() {
        pollJob = viewModelScope.launch {
            while (true) {
                val message = withContext(Dispatchers.IO) { client?.pollMessage() }
                if (message != null) {
                    messages.add(message)
                } else {
                    delay(250)
                }
            }
        }
    }

    private fun startDiscoveryPolling() {
        discoveryJob = viewModelScope.launch {
            while (true) {
                val current = client
                if (current != null) {
                    discoveredPeers = withContext(Dispatchers.IO) { current.discoveredPeers() }
                }
                delay(1500)
            }
        }
    }

    override fun onCleared() {
        disconnect()
        super.onCleared()
    }
}
