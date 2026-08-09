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

    private var client: MeshClient? = null
    private var pollJob: Job? = null

    /**
     * Starts (or restarts) a mesh node with the given settings.
     *
     * [peerAddrs] are directly-reachable peers on the same Wi-Fi network, e.g.
     * "192.168.1.42:9001". This is today's UDP transport; it will be replaceable with
     * Bluetooth LE auto-discovery once peripheral mode lands (see the repo roadmap).
     */
    fun connect(displayName: String, listenPort: Int, peerAddrs: List<String>, channel: String) {
        disconnect()
        try {
            val newClient = MeshClient(
                displayName = displayName,
                listenAddr = "0.0.0.0:$listenPort",
                peerAddrs = peerAddrs,
                channelPassphrase = channel,
                ttl = 16u,
            )
            client = newClient
            nodeId = newClient.nodeId()
            isConnected = true
            lastError = null
            startPolling()
        } catch (e: Exception) {
            lastError = e.message ?: e.toString()
            isConnected = false
        }
    }

    fun disconnect() {
        pollJob?.cancel()
        pollJob = null
        client = null
        isConnected = false
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

    override fun onCleared() {
        disconnect()
        super.onCleared()
    }
}
