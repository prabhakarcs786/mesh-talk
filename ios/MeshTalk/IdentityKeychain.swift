import Foundation
import Security

/// Persists this device's mesh identity seed (32 raw bytes -- see
/// `MeshClient.identitySeed()`) in the iOS Keychain, so the same `NodeId` survives across
/// app restarts instead of a brand-new random identity being generated every launch.
/// Treat this value like a private key: anyone with it can sign messages as this device.
///
/// Also persists the Milestone 3C.1 at-rest storage key protecting `InboxStore`'s
/// content (`MeshClient.inboxStorageKey()`) under a separate Keychain account -- same
/// mechanism, different secret, so losing/rotating one never affects the other.
enum IdentityKeychain {
    private static let service = "com.meshtalk.app.identity"
    private static let account = "mesh-identity-seed"
    private static let storageKeyAccount = "mesh-inbox-storage-key"

    /// Returns the previously-saved seed, or `nil` on first-ever launch (or if it was
    /// somehow cleared -- e.g. app reinstall, since Keychain items can outlive an app
    /// uninstall on iOS, but treating "missing" as "first launch" either way is safe).
    static func loadSeed() -> Data? {
        load(account: account)
    }

    /// Saves (or overwrites) the seed. Called after every `MeshClient` construction --
    /// idempotent whether the seed was freshly generated or reused from a previous
    /// launch, so there's no separate "first run" code path to get wrong.
    static func saveSeed(_ seed: Data) {
        save(seed, account: account)
    }

    /// Returns the previously-saved `InboxStore` at-rest storage key, or `nil` on
    /// first-ever launch. Treat this like a private key: anyone who obtains it can
    /// read this device's durably-stored chat history.
    static func loadStorageKey() -> Data? {
        load(account: storageKeyAccount)
    }

    /// Saves (or overwrites) the storage key -- same idempotent-every-launch pattern
    /// as `saveSeed`.
    static func saveStorageKey(_ key: Data) {
        save(key, account: storageKeyAccount)
    }

    private static func load(account: String) -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess else { return nil }
        return result as? Data
    }

    private static func save(_ value: Data, account: String) {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        // Keychain has no simple upsert -- delete any existing item first, then add.
        SecItemDelete(query as CFDictionary)

        var attributes = query
        attributes[kSecValueData as String] = value
        attributes[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        SecItemAdd(attributes as CFDictionary, nil)
    }
}

/// Converts a hex string (as returned by `MeshClient.identitySeed()`) back into raw bytes.
func dataFromHex(_ hex: String) -> Data? {
    guard hex.count % 2 == 0 else { return nil }
    var data = Data(capacity: hex.count / 2)
    var index = hex.startIndex
    while index < hex.endIndex {
        let next = hex.index(index, offsetBy: 2)
        guard let byte = UInt8(hex[index..<next], radix: 16) else { return nil }
        data.append(byte)
        index = next
    }
    return data
}
