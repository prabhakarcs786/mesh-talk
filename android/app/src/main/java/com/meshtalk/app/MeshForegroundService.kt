package com.meshtalk.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder

/**
 * Keeps the mesh node's process alive and responsive while the app is backgrounded.
 *
 * Without this, Android treats meshtalk like any other backgrounded app: its
 * coroutines/network sockets get throttled by Doze/App Standby, and on real devices
 * (especially with manufacturer battery-optimization on top of stock Android) the
 * whole process can be frozen or killed outright to reclaim memory -- which is exactly
 * what reproduced the reported bug ("call the emulator, answer, app closes"): the
 * *receiving* device had backgrounded the app before the call arrived, so by the time
 * the call signal came in there was no live [MeshStore]/[MeshClient] session left to
 * receive it, and the mesh connection itself had silently reset to "not connected".
 *
 * This app can't rely on push notifications (Firebase Cloud Messaging, APNs) to wake
 * itself up for an incoming message/call the way a normal chat app would -- it's a
 * fully offline, infrastructure-free mesh, so the *only* way to keep receiving
 * messages/calls while backgrounded is to hold a foreground service the whole time
 * the mesh node is running. This service intentionally does nothing else -- it holds
 * no reference to [MeshClient] itself (that still lives in [MeshStore], an
 * `AndroidViewModel`) -- its only job is to call [startForeground] so the OS treats
 * this whole process as user-visible/high-priority for as long as meshtalk is
 * "Started" in Settings, keeping [MeshStore]'s own coroutines and the underlying Rust
 * mesh node's sockets alive in the background.
 */
class MeshForegroundService : Service() {

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForegroundCompat()
        // START_STICKY: if the OS still kills this process under real memory
        // pressure despite the foreground priority, ask it to recreate the service
        // (with a null intent) once resources free up, rather than leaving meshtalk
        // silently stopped until the user manually reopens it.
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun startForegroundCompat() {
        val manager = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val existing = manager.getNotificationChannel(CHANNEL_ID)
            if (existing == null) {
                val channel = NotificationChannel(
                    CHANNEL_ID,
                    "Mesh connection",
                    NotificationManager.IMPORTANCE_LOW,
                ).apply {
                    description = "Keeps meshtalk connected to nearby devices in the background"
                }
                manager.createNotificationChannel(channel)
            }
        }

        val openAppIntent = packageManager.getLaunchIntentForPackage(packageName)
        val contentIntent = PendingIntent.getActivity(
            this,
            0,
            openAppIntent,
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
        )

        val notification: Notification = Notification.Builder(this, CHANNEL_ID)
            .setContentTitle("meshtalk")
            .setContentText("Connected to the mesh -- listening for messages and calls")
            .setSmallIcon(android.R.drawable.ic_menu_share)
            .setContentIntent(contentIntent)
            .setOngoing(true)
            .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(NOTIFICATION_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    companion object {
        private const val CHANNEL_ID = "mesh_connection"
        private const val NOTIFICATION_ID = 1

        fun start(context: Context) {
            val intent = Intent(context, MeshForegroundService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, MeshForegroundService::class.java))
        }
    }
}
