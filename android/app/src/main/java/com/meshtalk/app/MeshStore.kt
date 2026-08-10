package com.meshtalk.app

import android.app.Application
import android.util.Log
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.AndroidViewModel
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
import uniffi.mesh_mobile.ContactEventKind
import uniffi.mesh_mobile.ContactIdentity
import uniffi.mesh_mobile.DiscoveredPeer
import uniffi.mesh_mobile.FileAttachment
import uniffi.mesh_mobile.MeshClient
import uniffi.mesh_mobile.MeshClientConfig
import uniffi.mesh_mobile.ReceivedMessage
import uniffi.mesh_mobile.SendOutcome
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
class MeshStore(application: Application) : AndroidViewModel(application) {
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
     * Every contact whose cryptographic identity (X25519 key, verified via its Ed25519
     * binding) is currently known, keyed by full node id. This is what actually makes
     * [send]/[sendFile] work: unlike [onlinePeerIds], this persists for a contact seen
     * earlier even after they stop broadcasting (e.g. they're offline right now), which
     * is what lets "MeshTalk Direct Encryption v1" messages still be created for them.
     */
    val contacts = mutableStateMapOf<String, ContactIdentity>()

    /**
     * Full node ids whose cryptographic identity unexpectedly changed since it was first
     * seen -- surfaced prominently (never silently accepted) until the user dismisses
     * it. See [uniffi.mesh_mobile.ContactEventKind.IDENTITY_CHANGED].
     */
    var identityChangedPeerIds by mutableStateOf<Set<String>>(emptySet())
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
     * Where the persistent contact cache (Milestone 2B.2a) is stored -- the app's
     * internal (app-private) files directory, which Android never exposes to other
     * apps. A fixed, stable path (not tied to any particular [MeshClient] instance) so
     * a contact discovered before an app restart is still there afterward.
     */
    private fun contactsDbPath(): String {
        val context = getApplication<Application>()
        return java.io.File(context.filesDir, "contacts.json").absolutePath
    }

    /**
     * Where durable replay protection (Milestone 2D) is stored -- separate file from
     * [contactsDbPath], same app-private directory. Without this, an attacker (or a
     * relay redelivering a held copy) could replay an already-processed message just
     * by waiting for this device to restart.
     */
    private fun replayStorePath(): String {
        val context = getApplication<Application>()
        return java.io.File(context.filesDir, "replay-store.sqlite").absolutePath
    }

    /**
     * Where this device's own not-yet-acknowledged outgoing reliable messages and
     * their retry/backoff state (Milestone 3A) are stored -- without this, a message
     * still waiting for an ack when the app is killed wouldn't resume retrying after
     * the next launch.
     */
    private fun deliveryStorePath(): String {
        val context = getApplication<Application>()
        return java.io.File(context.filesDir, "delivery-store.sqlite").absolutePath
    }

    /**
     * Where this device's per-neighbor relay-forwarding retry state (Milestone 3B) is
     * stored -- relevant whenever this device is relaying `DirectV1` traffic for other
     * devices on the mesh, not only when it's the sender/recipient.
     */
    private fun forwardStorePath(): String {
        val context = getApplication<Application>()
        return java.io.File(context.filesDir, "forward-store.sqlite").absolutePath
    }

    /**
     * Where every durably-accepted received message is stored (Milestone 3C) -- the
     * actual source of truth for chat history, not just an in-memory cache. An
     * authenticated delivery ack is only ever sent once a message has been durably
     * written here.
     */
    private fun inboxStorePath(): String {
        val context = getApplication<Application>()
        return java.io.File(context.filesDir, "inbox-store.sqlite").absolutePath
    }

    /**
     * Starts a mesh node and immediately starts LAN auto-discovery, so nearby devices
     * running meshtalk on the same Wi-Fi network show up on their own -- connecting to
     * one is then a single tap ([connect]) instead of typing an IP address.
     */
    fun start(displayName: String, listenPort: Int, channel: String) {
        disconnect()
        try {
            val context = getApplication<Application>()
            // Bundled into a single MeshClientConfig record (rather than one
            // constructor parameter per field) to work around a known, unresolved
            // upstream uniffi-rs/JNA bug that corrupts many-struct-by-value native
            // calls specifically on Android ARM64 -- see the doc comment on
            // MeshClientConfig in crates/mesh-mobile/src/lib.rs, and
            // https://github.com/mozilla/uniffi-rs/issues/2624.
            val newClient = MeshClient(
                MeshClientConfig(
                    displayName = displayName,
                    listenAddr = "0.0.0.0:$listenPort",
                    peerAddrs = emptyList(),
                    channelPassphrase = channel,
                    ttl = 16u,
                    identitySeed = IdentityStore.loadSeed(context),
                    contactsDbPath = contactsDbPath(),
                    replayStorePath = replayStorePath(),
                    deliveryStorePath = deliveryStorePath(),
                    forwardStorePath = forwardStorePath(),
                    inboxStorePath = inboxStorePath(),
                    inboxStorageKey = IdentityStore.loadStorageKey(context),
                )
            )
            // Persist the seed regardless of whether it was just generated (first
            // launch) or reused (every launch after) -- keeps this device's NodeId
            // stable across restarts instead of silently becoming a new identity.
            byteArrayFromHex(newClient.identitySeed())?.let { IdentityStore.saveSeed(context, it) }
            // Milestone 3C.1: same idempotent-every-launch persistence for the inbox
            // at-rest storage key -- losing this makes previously-stored chat history
            // permanently unreadable, so it must be saved immediately, not only on
            // first-ever generation.
            byteArrayFromHex(newClient.inboxStorageKey())?.let { IdentityStore.saveStorageKey(context, it) }
            newClient.startDiscovery()
            client = newClient
            nodeId = newClient.nodeId()
            isConnected = true
            lastError = null
            // Hydrate from whatever the persistent ContactStore already had on disk --
            // e.g. a contact discovered before the app was last killed, or an
            // identity-change warning the user hasn't acknowledged yet -- so both
            // survive this restart instead of only reappearing once discovery happens
            // to see that peer again.
            val restoredContacts = newClient.contacts()
            contacts.clear()
            for (contact in restoredContacts) {
                contacts[contact.fullNodeId] = contact
            }
            identityChangedPeerIds = restoredContacts.filter { it.identityChangePending }.map { it.fullNodeId }.toSet()
            // Milestone 3C: chat history lives in the durable inbox, not an in-memory
            // list -- hydrate it from disk instead of starting empty every launch.
            messages.clear()
            messages.addAll(newClient.chatHistory())
            startPolling()
            startDiscoveryPolling()
            // Keeps this whole process alive/responsive while the app is backgrounded
            // -- see MeshForegroundService's doc comment for why this is required at
            // all (no push-notification service exists to wake a backgrounded process
            // back up for an incoming message/call in a fully offline P2P mesh).
            MeshForegroundService.start(context)
        } catch (e: Exception) {
            Log.e("MeshStore", "start() failed", e)
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
        // Bug: previously this only dropped the Kotlin reference (`client = null`)
        // without closing the underlying Rust object. UniFFI-generated objects are
        // only freed when the JVM's garbage collector gets around to it -- which is
        // non-deterministic and can take an arbitrary amount of time. In practice
        // that left the old MeshClient's Tokio runtime (and the UDP socket it had
        // bound) alive in the background, so an immediate "Restart" tapped right
        // after "Stop" would fail with "Address already in use (os error 98)" trying
        // to rebind the same port -- and, worse, the still-running old background
        // receive loop and call-handling state could race with the new client's,
        // which is a very plausible cause of the app crashing when receiving a call.
        // MeshClient.close() (UniFFI's generated AutoCloseable/Disposable) frees the
        // Rust side synchronously and is safe to call more than once.
        client?.close()
        client = null
        isConnected = false
        discoveredPeers = emptyList()
        connectedAddresses.clear()
        connectedPeers.clear()
        onlinePeerIds = emptySet()
        contacts.clear()
        identityChangedPeerIds = emptySet()
        endActiveCallResources()
        callPhase = CallPhase.Idle
        MeshForegroundService.stop(getApplication())
    }

    /** Sends a direct text message to one specific peer -- not a shared broadcast. */
    fun send(text: String, peerId: String) {
        val current = client ?: return
        if (text.isBlank()) return
        when (current.send(peerId, text)) {
            SendOutcome.SENT -> messages.add(ReceivedMessage(peerId, OWN_MESSAGE_SENDER_ID, java.util.UUID.randomUUID().toString(), text, null))
            SendOutcome.TOO_LONG_FOR_RELIABLE_TEXT ->
                // Milestone 3C: never silently downgrade to a weaker delivery
                // guarantee -- tell the user instead of quietly sending best-effort.
                lastError = "Message too long to send reliably -- try a shorter message."
            SendOutcome.FAILED ->
                lastError = if (isSecure(peerId)) {
                    "Failed to send -- no reachable peers right now."
                } else {
                    "Secure identity unavailable for this contact -- can't send yet."
                }
        }
    }

    /**
     * Sends an image, video, or voice note directly to one specific peer. Large
     * attachments (especially video) may not reliably arrive over many hops -- the mesh
     * has no retransmission -- so this works best for photos and short voice notes.
     */
    fun sendFile(data: ByteArray, fileName: String, mimeType: String, kind: AttachmentKind, peerId: String) {
        val current = client ?: return
        if (!current.sendFile(peerId, data, fileName, mimeType, kind)) {
            lastError = if (isSecure(peerId)) {
                "Failed to send attachment -- no reachable peers right now."
            } else {
                "Secure identity unavailable for this contact -- can't send yet."
            }
            return
        }
        val attachment = FileAttachment("", kind, fileName, mimeType, data)
        messages.add(ReceivedMessage(peerId, OWN_MESSAGE_SENDER_ID, java.util.UUID.randomUUID().toString(), null, attachment))
    }

    /**
     * Whether this device currently holds a verified cryptographic identity for
     * [peerId] -- i.e. whether "MeshTalk Direct Encryption v1" can actually be used to
     * message them right now. `false` means messages to them will fail closed rather
     * than send -- the UI should show something like "Secure identity unavailable"
     * rather than a generic send failure.
     */
    fun isSecure(peerId: String): Boolean = contacts.containsKey(peerId)

    /**
     * Call after showing an "identity changed" warning, so it stops reappearing.
     * Persists immediately via the Rust-side `ContactStore` so the acknowledgement
     * itself survives a restart too -- otherwise the warning would come back every time
     * the app relaunches.
     */
    fun acknowledgeIdentityChange(peerId: String) {
        identityChangedPeerIds = identityChangedPeerIds - peerId
        client?.acknowledgeIdentityChange(peerId)
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
                    // Milestone 3C: `mesh-core` already durably persisted this message
                    // (to `inboxStorePath`) and, for a `DirectV1` message, already
                    // acknowledged it to the sender -- there is nothing left to do here
                    // to make it durable.
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
                    drainContactEvents(current)
                }
                delay(1500)
            }
        }
    }

    /**
     * Drains new-contact/identity-change news and keeps [contacts] in sync with the
     * Rust-side cache -- called right after `discoveredPeers()` since that's what
     * actually updates the cache on the Rust side.
     */
    private suspend fun drainContactEvents(client: MeshClient) {
        var sawChangeOrNewContact = false
        while (true) {
            val event = withContext(Dispatchers.IO) { client.pollContactEvent() } ?: break
            sawChangeOrNewContact = true
            if (event.kind == ContactEventKind.IDENTITY_CHANGED) {
                identityChangedPeerIds = identityChangedPeerIds + event.fullNodeId
            }
        }
        if (sawChangeOrNewContact) {
            val updated = withContext(Dispatchers.IO) { client.contacts() }
            contacts.clear()
            for (contact in updated) {
                contacts[contact.fullNodeId] = contact
            }
        }
    }

    override fun onCleared() {
        disconnect()
        super.onCleared()
    }
}
