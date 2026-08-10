package com.meshtalk.app

import android.content.Context
import android.content.SharedPreferences
import androidx.security.crypto.EncryptedSharedPreferences
import androidx.security.crypto.MasterKey
import android.util.Base64

/**
 * Persists this device's mesh identity seed (32 raw bytes -- see
 * `MeshClient.identitySeed()`) in Keystore-backed encrypted storage, so the same
 * `NodeId` survives across app restarts instead of a brand-new random identity being
 * generated every launch. Treat this value like a private key: anyone with it can sign
 * messages as this device.
 *
 * Also persists the Milestone 3C.1 at-rest storage key protecting `InboxStore`'s
 * content (`MeshClient.inboxStorageKey()`) under a separate preference entry -- same
 * Keystore-backed mechanism, different secret, so losing/rotating one never affects
 * the other.
 */
object IdentityStore {
    private const val PREFS_NAME = "mesh_identity_store"
    private const val SEED_KEY = "identity_seed"
    private const val STORAGE_KEY_KEY = "inbox_storage_key"

    private fun prefs(context: Context): SharedPreferences {
        val masterKey = MasterKey.Builder(context)
            .setKeyScheme(MasterKey.KeyScheme.AES256_GCM)
            .build()
        return EncryptedSharedPreferences.create(
            context,
            PREFS_NAME,
            masterKey,
            EncryptedSharedPreferences.PrefKeyEncryptionScheme.AES256_SIV,
            EncryptedSharedPreferences.PrefValueEncryptionScheme.AES256_GCM,
        )
    }

    /** Returns the previously-saved seed, or `null` on first-ever launch. */
    fun loadSeed(context: Context): ByteArray? {
        val encoded = prefs(context).getString(SEED_KEY, null) ?: return null
        return Base64.decode(encoded, Base64.NO_WRAP)
    }

    /**
     * Saves (or overwrites) the seed. Called after every `MeshClient` construction --
     * idempotent whether the seed was freshly generated or reused from a previous
     * launch, so there's no separate "first run" code path to get wrong.
     */
    fun saveSeed(context: Context, seed: ByteArray) {
        prefs(context).edit().putString(SEED_KEY, Base64.encodeToString(seed, Base64.NO_WRAP)).apply()
    }

    /**
     * Returns the previously-saved `InboxStore` at-rest storage key, or `null` on
     * first-ever launch. Treat this like a private key: anyone who obtains it can read
     * this device's durably-stored chat history.
     */
    fun loadStorageKey(context: Context): ByteArray? {
        val encoded = prefs(context).getString(STORAGE_KEY_KEY, null) ?: return null
        return Base64.decode(encoded, Base64.NO_WRAP)
    }

    /** Saves (or overwrites) the storage key -- same idempotent-every-launch pattern as [saveSeed]. */
    fun saveStorageKey(context: Context, key: ByteArray) {
        prefs(context).edit().putString(STORAGE_KEY_KEY, Base64.encodeToString(key, Base64.NO_WRAP)).apply()
    }
}

/** Converts a hex string (as returned by `MeshClient.identitySeed()`) back into raw bytes. */
fun byteArrayFromHex(hex: String): ByteArray? {
    if (hex.length % 2 != 0) return null
    return try {
        ByteArray(hex.length / 2) { i ->
            hex.substring(i * 2, i * 2 + 2).toInt(16).toByte()
        }
    } catch (e: NumberFormatException) {
        null
    }
}
