package com.meshtalk.app

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import uniffi.mesh_mobile.AttachmentKind
import uniffi.mesh_mobile.CallEvent
import uniffi.mesh_mobile.CallEventKind
import uniffi.mesh_mobile.CallMediaKind
import uniffi.mesh_mobile.DiscoveredPeer
import uniffi.mesh_mobile.FileAttachment
import uniffi.mesh_mobile.MeshClient
import uniffi.mesh_mobile.ReceivedMessage
import uniffi.mesh_mobile.TransferProgressUpdate

/**
 * Sentinel `senderId` used for messages/attachments this device sent itself, appended
 * locally (not received back over the mesh) so the sender sees their own outgoing
 * messages in their own chat history too, same as a normal chat app.
 */
const val OWN_MESSAGE_SENDER_ID = "me"

/**
 * Where a call currently stands. `callId`/`remoteNodeId` are the hex ids needed to
 * address further signaling (`acceptCall`/`rejectCall`/`endCall`) or frames at the right
 * party. Mirrors `ios/MeshTalk/MeshStore.swift`'s `CallPhase`.
 */
sealed class CallPhase {
    object Idle : CallPhase()
    data class OutgoingRinging(val remoteNodeId: String, val remoteName: String, val callId: String, val video: Boolean) : CallPhase()
    data class IncomingRinging(val remoteNodeId: String, val remoteName: String, val callId: String, val video: Boolean) : CallPhase()
    data class Active(
        val remoteNodeId: String,
        val remoteName: String,
        val callId: String,
        val video: Boolean,
        val startedAtMs: Long,
    ) : CallPhase()
}

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

    /**
     * Full node ids currently visible via LAN auto-discovery. A peer drops out of this
     * set once discovery stops seeing broadcasts from it (e.g. it went out of range or
     * closed the app), which is what drives the online/offline indicator per contact.
     */
    var onlinePeerIds by mutableStateOf<Set<String>>(emptySet())
        private set

    /**
     * Live progress for attachments currently being sent or received, keyed by transfer
     * id -- lets the chat screen show a progress bar instead of the attachment appearing
     * to do nothing until it's 100% there. Entries are removed once a send finishes, or
     * once the fully-reassembled attachment shows up in [messages].
     */
    val activeTransfers = mutableStateMapOf<String, TransferProgressUpdate>()
    /** Insertion order for [activeTransfers], since maps don't preserve one. */
    val activeTransferOrder = mutableStateListOf<String>()

    /** Peers we've connected to, keyed by full node id -- so we know who to call. */
    val connectedPeers = mutableStateMapOf<String, DiscoveredPeer>()

    /** Current call state -- drives the incoming-call banner and in-call screen. */
    var callPhase by mutableStateOf<CallPhase>(CallPhase.Idle)
        private set
    /** Latest received remote video frame for an active video call (null if audio-only). */
    var remoteVideoFrame by mutableStateOf<ByteArray?>(null)
        private set
    var isMuted by mutableStateOf(false)
        private set

    private var client: MeshClient? = null
    private var pollJob: Job? = null
    private var discoveryJob: Job? = null
    private var callFrameJob: Job? = null
    private var callAudio: CallAudioEngine? = null

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
        connectedPeers[peer.fullNodeId] = peer
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
        connectedPeers.clear()
        onlinePeerIds = emptySet()
        endActiveCallResources()
        callPhase = CallPhase.Idle
    }

    /** Sends a direct text message to one specific peer -- not a shared broadcast. */
    fun send(text: String, peerId: String) {
        val current = client ?: return
        if (text.isBlank()) return
        if (!current.send(peerId, text)) {
            lastError = "Failed to send -- no reachable peers right now."
            return
        }
        messages.add(ReceivedMessage(peerId, OWN_MESSAGE_SENDER_ID, text, null))
    }

    /**
     * Sends an image, video, or voice note directly to one specific peer. Large
     * attachments (especially video) may not reliably arrive over many hops -- the mesh
     * has no retransmission -- so this works best for photos and short voice notes.
     */
    fun sendFile(data: ByteArray, fileName: String, mimeType: String, kind: AttachmentKind, peerId: String) {
        val current = client ?: return
        if (!current.sendFile(peerId, data, fileName, mimeType, kind)) {
            lastError = "Failed to send attachment -- no reachable peers right now."
            return
        }
        val attachment = FileAttachment("", kind, fileName, mimeType, data)
        messages.add(ReceivedMessage(peerId, OWN_MESSAGE_SENDER_ID, null, attachment))
    }

    /** Messages exchanged with just this one peer, in arrival order. */
    fun messagesWith(peerId: String): List<ReceivedMessage> = messages.filter { it.peerId == peerId }

    /** Short subtitle for a conversation-list card: last text, or a label for attachments. */
    fun lastMessagePreview(peerId: String): String? {
        val last = messages.lastOrNull { it.peerId == peerId } ?: return null
        last.text?.let { return it }
        val attachment = last.attachment ?: return null
        return when (attachment.kind) {
            AttachmentKind.IMAGE -> "Photo"
            AttachmentKind.VIDEO -> "Video"
            AttachmentKind.VOICE -> "Voice note"
            AttachmentKind.FILE -> attachment.name
        }
    }

    fun isOnline(peerId: String): Boolean = onlinePeerIds.contains(peerId)

    // MARK: - Calling

    /** Places a voice or video call to an already-connected peer. No-op if mid-call. */
    fun placeCall(peer: DiscoveredPeer, video: Boolean) {
        val current = client ?: return
        if (callPhase != CallPhase.Idle) return
        val callId = current.startCall(peer.fullNodeId, video)
        if (callId == null) {
            lastError = "Couldn't start the call."
            return
        }
        callPhase = CallPhase.OutgoingRinging(peer.fullNodeId, peer.displayName, callId, video)
    }

    /** Answers the current incoming call. */
    fun acceptIncomingCall() {
        val phase = callPhase
        val current = client ?: return
        if (phase !is CallPhase.IncomingRinging) return
        current.acceptCall(phase.remoteNodeId, phase.callId)
        startActiveCall(phase.remoteNodeId, phase.remoteName, phase.callId, phase.video)
    }

    /** Declines the current incoming call. */
    fun rejectIncomingCall() {
        val phase = callPhase
        if (phase !is CallPhase.IncomingRinging) return
        client?.rejectCall(phase.remoteNodeId, phase.callId)
        callPhase = CallPhase.Idle
    }

    /** Ends the call in progress, or cancels one that hasn't been answered yet. */
    fun hangUp() {
        when (val phase = callPhase) {
            is CallPhase.Active -> client?.endCall(phase.remoteNodeId, phase.callId)
            is CallPhase.OutgoingRinging -> client?.endCall(phase.remoteNodeId, phase.callId)
            else -> {}
        }
        endActiveCallResources()
        callPhase = CallPhase.Idle
    }

    fun toggleMute() {
        isMuted = !isMuted
    }

    /**
     * Forwards one captured video frame for the active call. The `CallVideoCapture` (and
     * the Android `Context`/`LifecycleOwner` it needs) is owned by the call UI, not this
     * view model -- see `CallScreen.kt` -- so this is how it reaches the network.
     */
    fun sendActiveCallVideoFrame(sequence: Int, data: ByteArray) {
        val phase = callPhase
        if (phase !is CallPhase.Active) return
        client?.sendCallFrame(phase.remoteNodeId, phase.callId, CallMediaKind.VIDEO, sequence.toUInt(), data)
    }

    private fun startActiveCall(remoteNodeId: String, name: String, callId: String, video: Boolean) {
        callPhase = CallPhase.Active(remoteNodeId, name, callId, video, System.currentTimeMillis())
        isMuted = false

        val audio = CallAudioEngine { sequence, data ->
            if (!isMuted) {
                client?.sendCallFrame(remoteNodeId, callId, CallMediaKind.AUDIO, sequence.toUInt(), data)
            }
        }
        audio.start()
        callAudio = audio

        callFrameJob = viewModelScope.launch {
            while (true) {
                val frame = withContext(Dispatchers.IO) { client?.pollCallFrame() }
                if (frame != null) {
                    when (frame.media) {
                        CallMediaKind.AUDIO -> callAudio?.play(frame.data)
                        CallMediaKind.VIDEO -> remoteVideoFrame = frame.data
                    }
                } else {
                    delay(10)
                }
            }
        }
    }

    private fun endActiveCallResources() {
        callFrameJob?.cancel()
        callFrameJob = null
        callAudio?.stop()
        callAudio = null
        remoteVideoFrame = null
    }

    private fun handleCallEvent(event: CallEvent) {
        when (val kind = event.kind) {
            is CallEventKind.IncomingInvite -> {
                if (callPhase != CallPhase.Idle) {
                    // Already on a call -- auto-decline instead of showing a second banner.
                    client?.rejectCall(event.remoteNodeId, event.callId)
                    return
                }
                val name = connectedPeers[event.remoteNodeId]?.displayName ?: event.remoteShortId
                callPhase = CallPhase.IncomingRinging(event.remoteNodeId, name, event.callId, kind.video)
            }
            is CallEventKind.Accepted -> {
                val phase = callPhase
                if (phase is CallPhase.OutgoingRinging && phase.callId == event.callId) {
                    startActiveCall(phase.remoteNodeId, phase.remoteName, phase.callId, phase.video)
                }
            }
            is CallEventKind.Rejected, is CallEventKind.Ended -> {
                val currentCallId = when (val phase = callPhase) {
                    is CallPhase.OutgoingRinging -> phase.callId
                    is CallPhase.IncomingRinging -> phase.callId
                    is CallPhase.Active -> phase.callId
                    CallPhase.Idle -> null
                }
                if (currentCallId == event.callId) {
                    endActiveCallResources()
                    callPhase = CallPhase.Idle
                }
            }
        }
    }

    private fun startPolling() {
        pollJob = viewModelScope.launch {
            while (true) {
                val current = client
                if (current == null) {
                    delay(250)
                    continue
                }
                // Track transfers that finished *this* poll so a stale progress update
                // from just before completion (still queued when the completed message
                // was already drained) doesn't resurrect the progress bar for an
                // attachment that's already fully arrived.
                val justCompleted = mutableSetOf<String>()
                var didWork = false

                while (true) {
                    val message = withContext(Dispatchers.IO) { current.pollMessage() } ?: break
                    didWork = true
                    message.attachment?.transferId?.let {
                        removeActiveTransfer(it)
                        justCompleted.add(it)
                    }
                    messages.add(message)
                }
                while (true) {
                    val progress = withContext(Dispatchers.IO) { current.pollTransferProgress() } ?: break
                    didWork = true
                    if (justCompleted.contains(progress.transferId)) continue
                    if (progress.doneChunks >= progress.totalChunks) {
                        removeActiveTransfer(progress.transferId)
                        justCompleted.add(progress.transferId)
                    } else {
                        if (!activeTransfers.containsKey(progress.transferId)) {
                            activeTransferOrder.add(progress.transferId)
                        }
                        activeTransfers[progress.transferId] = progress
                    }
                }

                while (true) {
                    val event = withContext(Dispatchers.IO) { current.pollCallEvent() } ?: break
                    didWork = true
                    handleCallEvent(event)
                }

                if (!didWork) {
                    delay(250)
                }
            }
        }
    }

    private fun removeActiveTransfer(transferId: String) {
        activeTransfers.remove(transferId)
        activeTransferOrder.remove(transferId)
    }

    private fun startDiscoveryPolling() {
        discoveryJob = viewModelScope.launch {
            while (true) {
                val current = client
                if (current != null) {
                    val peers = withContext(Dispatchers.IO) { current.discoveredPeers() }
                    discoveredPeers = peers
                    onlinePeerIds = peers.map { it.fullNodeId }.toSet()
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
