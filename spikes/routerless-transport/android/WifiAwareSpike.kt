// Milestone 4A, Phase 2 -- DRAFT, UNTESTED. See ../README.md before using this file.
//
// Minimal Wi-Fi Aware publish/subscribe/connect spike for Android 8.0 (API 26)+,
// devices with Wi-Fi Aware hardware actually present (chipset-dependent -- must be
// runtime-checked, see checkWifiAwareAvailable() below). Proves nothing about
// MeshTalk's mesh protocol -- it exchanges exactly one fixed literal string,
// MESHTALK_ROUTERLESS_TEST_V1, over a real Socket/ServerSocket carried across a
// Wi-Fi Aware network connection, per Milestone 4A Phase 2's scope.
//
// Every API used here is taken directly from Android's own "Wi-Fi Aware overview"
// guide (https://developer.android.com/develop/connectivity/wifi/wifi-aware),
// specifically its "Initial setup", "Obtain a session", "Publish a service",
// "Subscribe to a service", "Send a message", and "Create a connection" sections --
// not guessed. This file follows that guide's 8-step connection recipe exactly:
//   1. Publisher publishes a service; subscriber subscribes to it.
//   2. Subscriber discovers publisher, sends it a short message.
//   3. Publisher opens a ServerSocket, gets its port.
//   4. Publisher requests a Wi-Fi Aware network via ConnectivityManager +
//      WifiAwareNetworkSpecifier(discoverySession, subscriberPeerHandle, port).
//   5. Once the publisher's network request is up, it sends a message back to the
//      subscriber (the guide doesn't specify content here; this file reuses the
//      publisher's own PeerHandle-independent port number so the subscriber knows
//      networking is ready).
//   6. Subscriber receives that message, then requests its own Wi-Fi Aware network
//      (same API, no port).
//   7. Subscriber's onAvailable()/onCapabilitiesChanged() gives it the publisher's
//      IPv6 address + port via WifiAwareNetworkInfo -- opens a real Socket to the
//      ServerSocket.
//   8. Fixed test string is written/read over that socket; connections are closed.
//
// Prerequisites this file assumes but does NOT set up (see ../README.md):
//   - Manifest permissions listed in docs/routerless-transport-capability-matrix.md
//     (ACCESS_WIFI_STATE, CHANGE_WIFI_STATE, CHANGE_NETWORK_STATE, INTERNET,
//     NEARBY_WIFI_DEVICES for API 33+, ACCESS_FINE_LOCATION up to API 32) are NOT
//     currently declared in android/app/src/main/AndroidManifest.xml -- add them to
//     a throwaway module/debug screen, not the shipping manifest, until Phase 2
//     actually passes.
//   - Never compiled, never run. Treat every line as unverified until it has been
//     built and exercised on two physical devices with confirmed Wi-Fi Aware
//     hardware.

package com.meshtalk.spike.routerless

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.PackageManager
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.aware.DiscoverySessionCallback
import android.net.wifi.aware.PeerHandle
import android.net.wifi.aware.PublishConfig
import android.net.wifi.aware.PublishDiscoverySession
import android.net.wifi.aware.SubscribeConfig
import android.net.wifi.aware.SubscribeDiscoverySession
import android.net.wifi.aware.WifiAwareManager
import android.net.wifi.aware.WifiAwareNetworkInfo
import android.net.wifi.aware.WifiAwareNetworkSpecifier
import android.net.wifi.aware.WifiAwareSession
import android.util.Log
import java.net.ServerSocket
import java.net.Socket
import java.nio.charset.StandardCharsets

private const val TAG = "MeshTalkRoutlerlessSpike"

/** Must match on both publisher and subscriber. */
private const val SERVICE_NAME = "MeshTalkRoutlerlessSpikeV1"

/** Fixed test payload for Milestone 4A Phase 2 -- deliberately not mesh-protocol content. */
private const val TEST_PAYLOAD = "MESHTALK_ROUTERLESS_TEST_V1"

/** Placeholder shared secret for the Wi-Fi Aware network -- a real integration must
 * not hardcode this; not in Phase 2's scope to fix. */
private const val SPIKE_PSK = "meshtalk-routerless-spike-placeholder"

/**
 * Step 0 (not in Android's numbered recipe, but required before any of it): confirm
 * the device actually has Wi-Fi Aware hardware available right now. Per the guide,
 * `hasSystemFeature` can be true (OS supports the API) while `isAvailable()` is false
 * (Wi-Fi/Location off, or hardware busy with Wi-Fi Direct/SoftAP/tethering).
 */
fun checkWifiAwareAvailable(context: Context): Boolean {
    val hasFeature = context.packageManager.hasSystemFeature(PackageManager.FEATURE_WIFI_AWARE)
    val manager = context.getSystemService(Context.WIFI_AWARE_SERVICE) as? WifiAwareManager
    val available = manager?.isAvailable == true
    Log.i(TAG, "hasSystemFeature=$hasFeature isAvailable=$available")
    return hasFeature && available
}

/**
 * Registers for WifiAwareManager.ACTION_WIFI_AWARE_STATE_CHANGED, per the guide's
 * instruction that availability can change at any time and existing sessions should
 * be discarded when it does. Caller owns unregistering this receiver.
 */
fun registerAvailabilityReceiver(context: Context, onChanged: (Boolean) -> Unit): BroadcastReceiver {
    val manager = context.getSystemService(Context.WIFI_AWARE_SERVICE) as? WifiAwareManager
    val receiver = object : BroadcastReceiver() {
        override fun onReceive(ctx: Context, intent: Intent) {
            onChanged(manager?.isAvailable == true)
        }
    }
    context.registerReceiver(receiver, IntentFilter(WifiAwareManager.ACTION_WIFI_AWARE_STATE_CHANGED))
    return receiver
}

/**
 * Publisher ("server") role. Publishes SERVICE_NAME, waits for the subscriber's
 * initial message, opens a ServerSocket, and requests a Wi-Fi Aware network scoped
 * to that specific subscriber's PeerHandle + the socket's port.
 */
class SpikePublisher(private val context: Context) {

    private var session: WifiAwareSession? = null
    private var discoverySession: PublishDiscoverySession? = null
    private var serverSocket: ServerSocket? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null

    fun start(onTestStringReceived: (String) -> Unit) {
        val manager = context.getSystemService(Context.WIFI_AWARE_SERVICE) as WifiAwareManager
        manager.attach(object : android.net.wifi.aware.AttachCallback() {
            override fun onAttached(awareSession: WifiAwareSession) {
                session = awareSession
                publish(awareSession, onTestStringReceived)
            }

            override fun onAttachFailed() {
                Log.e(TAG, "publisher: attach failed")
            }
        }, null)
    }

    private fun publish(awareSession: WifiAwareSession, onTestStringReceived: (String) -> Unit) {
        val config = PublishConfig.Builder()
            .setServiceName(SERVICE_NAME)
            .build()
        awareSession.publish(config, object : DiscoverySessionCallback() {
            override fun onPublishStarted(publishSession: PublishDiscoverySession) {
                Log.i(TAG, "publisher: publish started")
                discoverySession = publishSession
            }

            override fun onMessageReceived(peerHandle: PeerHandle, message: ByteArray) {
                // Subscriber's initial "hello" message (step 2). Now open the
                // ServerSocket and request the Wi-Fi Aware network (steps 3-4).
                Log.i(TAG, "publisher: message from subscriber: ${String(message, StandardCharsets.UTF_8)}")
                openServerSocketAndRequestNetwork(peerHandle, onTestStringReceived)
            }
        }, null)
    }

    private fun openServerSocketAndRequestNetwork(peerHandle: PeerHandle, onTestStringReceived: (String) -> Unit) {
        val ss = ServerSocket(0)
        serverSocket = ss
        val port = ss.localPort

        val ds = discoverySession ?: run {
            Log.e(TAG, "publisher: no discovery session, aborting")
            return
        }

        val networkSpecifier = WifiAwareNetworkSpecifier.Builder(ds, peerHandle)
            .setPskPassphrase(SPIKE_PSK)
            .setPort(port)
            .build()
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI_AWARE)
            .setNetworkSpecifier(networkSpecifier)
            .build()

        val connMgr = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                Log.i(TAG, "publisher: network available, sending port back to subscriber")
                // Step 5: tell the subscriber networking is ready. The guide leaves
                // message content to the app; this spike just re-sends the port as
                // ASCII so the subscriber has confirmation (it already knows the
                // port from this same message in a from-scratch design, but the
                // guide's step 5 explicitly calls for a message here).
                ds.sendMessage(peerHandle, /* messageId = */ 1, port.toString().toByteArray(StandardCharsets.UTF_8))

                Thread {
                    try {
                        val client = ss.accept()
                        val received = client.getInputStream().bufferedReader().readLine()
                        Log.i(TAG, "publisher: received over socket: $received")
                        if (received != null) onTestStringReceived(received)
                        client.getOutputStream().write((received ?: "").toByteArray(StandardCharsets.UTF_8))
                        client.close()
                    } catch (e: Exception) {
                        Log.e(TAG, "publisher: socket accept/read failed", e)
                    }
                }.start()
            }
        }
        networkCallback = callback
        connMgr.requestNetwork(request, callback)
    }

    fun stop() {
        val connMgr = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        networkCallback?.let { connMgr.unregisterNetworkCallback(it) }
        serverSocket?.close()
        discoverySession?.close()
        session?.close()
    }
}

/**
 * Subscriber ("client") role. Subscribes to SERVICE_NAME, sends the discovered
 * publisher a "hello" message, waits for its port confirmation, requests its own
 * Wi-Fi Aware network, then opens a Socket to the publisher's ServerSocket and
 * writes TEST_PAYLOAD.
 */
class SpikeSubscriber(private val context: Context) {

    private var session: WifiAwareSession? = null
    private var discoverySession: SubscribeDiscoverySession? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null

    fun start(onEchoReceived: (String) -> Unit) {
        val manager = context.getSystemService(Context.WIFI_AWARE_SERVICE) as WifiAwareManager
        manager.attach(object : android.net.wifi.aware.AttachCallback() {
            override fun onAttached(awareSession: WifiAwareSession) {
                session = awareSession
                subscribe(awareSession, onEchoReceived)
            }

            override fun onAttachFailed() {
                Log.e(TAG, "subscriber: attach failed")
            }
        }, null)
    }

    private fun subscribe(awareSession: WifiAwareSession, onEchoReceived: (String) -> Unit) {
        val config = SubscribeConfig.Builder()
            .setServiceName(SERVICE_NAME)
            .build()
        awareSession.subscribe(config, object : DiscoverySessionCallback() {
            override fun onSubscribeStarted(subscribeSession: SubscribeDiscoverySession) {
                Log.i(TAG, "subscriber: subscribe started")
                discoverySession = subscribeSession
            }

            override fun onServiceDiscovered(peerHandle: PeerHandle, serviceSpecificInfo: ByteArray, matchFilter: List<ByteArray>) {
                Log.i(TAG, "subscriber: discovered publisher")
                // Step 2: send the publisher a short "hello" message.
                discoverySession?.sendMessage(peerHandle, /* messageId = */ 1, "hello".toByteArray(StandardCharsets.UTF_8))
            }

            override fun onMessageReceived(peerHandle: PeerHandle, message: ByteArray) {
                // Step 6: publisher's port-confirmation message arrived. Now request
                // our own Wi-Fi Aware network (no port specified, per the guide).
                Log.i(TAG, "subscriber: got publisher confirmation: ${String(message, StandardCharsets.UTF_8)}")
                requestNetworkAndSend(peerHandle, onEchoReceived)
            }
        }, null)
    }

    private fun requestNetworkAndSend(peerHandle: PeerHandle, onEchoReceived: (String) -> Unit) {
        val ds = discoverySession ?: run {
            Log.e(TAG, "subscriber: no discovery session, aborting")
            return
        }

        val networkSpecifier = WifiAwareNetworkSpecifier.Builder(ds, peerHandle)
            .setPskPassphrase(SPIKE_PSK)
            .build()
        val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_WIFI_AWARE)
            .setNetworkSpecifier(networkSpecifier)
            .build()

        val connMgr = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onCapabilitiesChanged(network: Network, networkCapabilities: NetworkCapabilities) {
                // Step 7: pull the publisher's IPv6 + port from WifiAwareNetworkInfo,
                // open a real Socket, write the fixed test payload, read the echo.
                val peerAwareInfo = networkCapabilities.transportInfo as? WifiAwareNetworkInfo ?: return
                val peerIpv6 = peerAwareInfo.peerIpv6Addr ?: return
                val peerPort = peerAwareInfo.port

                Thread {
                    try {
                        val socket: Socket = network.socketFactory.createSocket(peerIpv6, peerPort)
                        socket.getOutputStream().write((TEST_PAYLOAD + "\n").toByteArray(StandardCharsets.UTF_8))
                        val echoed = socket.getInputStream().bufferedReader().readLine()
                        Log.i(TAG, "subscriber: echoed back: $echoed")
                        if (echoed != null) onEchoReceived(echoed)
                        socket.close()
                    } catch (e: Exception) {
                        Log.e(TAG, "subscriber: socket connect/write failed", e)
                    }
                }.start()
            }
        }
        networkCallback = callback
        connMgr.requestNetwork(request, callback)
    }

    fun stop() {
        val connMgr = context.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
        networkCallback?.let { connMgr.unregisterNetworkCallback(it) }
        discoverySession?.close()
        session?.close()
    }
}
