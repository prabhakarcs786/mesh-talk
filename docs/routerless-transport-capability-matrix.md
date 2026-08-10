# Routerless Transport Capability Matrix (Milestone 4A, Phase 1)

**Purpose:** before writing a single line of a new `Transport` implementation, establish
-- from official platform documentation only, never guessed -- which direct
device-to-device radio technologies MeshTalk could plausibly use with **no internet, no
cellular data, no personal hotspot, and no shared Wi-Fi router**, and what each one
actually requires (entitlements, permissions, minimum OS/hardware, background behavior).

This document does not implement anything. It is the input to the Phase 2 spike
decision. Nothing in this table has been verified on physical hardware yet -- see
"Verification status" per row. Anything not directly sourced from an official Apple/
Android document is marked "NEEDS VERIFICATION," never asserted as fact.

## Sources consulted

- Apple, [TN3111: iOS Wi-Fi API overview](https://developer.apple.com/documentation/technotes/tn3111-ios-wifi-api-overview) (peer-to-peer networking section, revised 2025-08-29 for iOS 26 Wi-Fi Aware)
- Apple, [Adopting Wi-Fi Aware](https://developer.apple.com/documentation/WiFiAware/Adopting-Wi-Fi-Aware)
- Android, [Connectivity guides overview](https://developer.android.com/develop/connectivity/overview)
- Android, [Wi-Fi Aware overview](https://developer.android.com/develop/connectivity/wifi/wifi-aware)
- Android, [Use Wi-Fi Direct (P2P) for service discovery](https://developer.android.com/develop/connectivity/wifi/nsd-wifi-direct)
- This repo's own `crates/mesh-transport-ble` and `README.md` (existing, already-verified BLE status)

---

## Summary table

| Capability | Android | iOS | Purpose | Cross-platform? |
|---|---|---|---|---|
| Wi-Fi Aware (NAN) | ✅ API 26+ (2017), chipset-dependent | ✅ iOS/iPadOS 26+, iPhone 12 and later only | High-throughput P2P data path, no AP needed | **Industry standard on both sides -- interop NOT yet proven, see Phase 2** |
| Wi-Fi Direct (P2P) | ✅ long-standing, device-dependent | ❌ no public API | Direct P2P without AP/hotspot | Android-only |
| Apple peer-to-peer Wi-Fi | ❌ N/A | ✅ iOS 7+, all devices | Legacy P2P (Multipeer Connectivity / Network framework `.p2p` service type) | **Apple documents this as effectively Apple-devices-only for third parties** |
| BLE (central + data) | ✅ | ✅ | Discovery, control messages, small payloads | Yes -- already this repo's `mesh-transport-ble` (central-only so far) |
| BLE peripheral/advertising | ⚠️ needs native code per OS (already noted in repo README) | ⚠️ same | Being *discoverable* over BLE | Yes, but not yet implemented either side |
| LAN UDP (existing) | ✅ | ✅ | Current transport | Requires a shared router/AP -- **not routerless** |

---

## Android

### Wi-Fi Aware
- **Availability**: devices running Android 8.0 (API level 26) and higher *may* support
  it; it is **not guaranteed on all devices** -- hardware/chipset-dependent. Must be
  checked at runtime, not assumed from OS version alone.
- **Runtime detection** (both required):
  1. `context.packageManager.hasSystemFeature(PackageManager.FEATURE_WIFI_AWARE)`
  2. `WifiAwareManager.isAvailable()` -- can be `false` even if the feature exists (Wi-Fi
     or Location disabled, or -- per Android's own docs -- some devices don't support
     Wi-Fi Aware concurrently with Wi-Fi Direct/SoftAP/tethering being active). Must also
     listen for `ACTION_WIFI_AWARE_STATE_CHANGED`, since availability can change at any
     time.
- **Permissions**: `ACCESS_WIFI_STATE`, `CHANGE_WIFI_STATE`, `CHANGE_NETWORK_STATE`,
  `INTERNET` (yes, even though no actual internet connectivity is used or required --
  it's needed for the underlying socket APIs), `NEARBY_WIFI_DEVICES` (required if
  targeting API 33+), `ACCESS_FINE_LOCATION` (required up to API 32).
- **Data model**: publish/subscribe discovery (service name string) → either short
  `sendMessage()` messages (Android's own docs: "limited to about 255 bytes," exact
  limit via `Characteristics.getMaxServiceSpecificInfoLength()`, and messages "might not
  be delivered, or be delivered out-of-order or more than once") **or** a full
  `ConnectivityManager`/`WifiAwareNetworkSpecifier`-negotiated network path carrying a
  real `Socket`/`ServerSocket` for actual bulk data -- MeshTalk would need the latter,
  not the message API, for anything beyond a tiny control message. **Confirmed from
  Android's own guide** (full 8-step recipe read and followed exactly, not guessed):
  publisher calls `WifiAwareSession.publish(PublishConfig, DiscoverySessionCallback)`;
  subscriber calls `subscribe(SubscribeConfig, DiscoverySessionCallback)`; on
  `onServiceDiscovered(peerHandle, ...)` the subscriber sends the publisher a short
  message via `DiscoverySession.sendMessage(peerHandle, messageId, bytes)`; the
  publisher's `onMessageReceived` then opens a `ServerSocket(0)`, and both sides
  independently call `ConnectivityManager.requestNetwork(...)` with a
  `NetworkRequest` carrying a `WifiAwareNetworkSpecifier.Builder(discoverySession,
  peerHandle)` (publisher also calls `.setPort(port)`); once available, the
  subscriber reads the publisher's IPv6 address/port from
  `(networkCapabilities.transportInfo as WifiAwareNetworkInfo)` and opens a plain
  `Socket` to it. No OS-level "pairing" UI step exists in this model, unlike iOS
  (see below) -- publish/subscribe/discover/connect all happen programmatically.
- **Background restrictions**: NEEDS VERIFICATION -- not covered in the pages fetched
  for this matrix; standard Android background-execution limits likely apply to
  maintaining an open discovery session/network while backgrounded, but this must be
  tested, not assumed.
- **Min supported OS**: API 26 (Android 8.0), but see "chipset-dependent" above.

### Wi-Fi Direct (P2P)
- Long-standing API (`WifiP2pManager`), works without any AP/hotspot/internet, but not
  every Android device supports it (`onFailure(P2P_UNSUPPORTED)` is a documented,
  expected outcome your code must handle).
- **Permissions**: `CHANGE_WIFI_STATE`, `ACCESS_WIFI_STATE`, `ACCESS_FINE_LOCATION`,
  `INTERNET`, plus `NEARBY_WIFI_DEVICES` for API 33+. Discovery calls
  (`discoverPeers`/`discoverServices`/`requestPeers`) additionally require Location Mode
  to be enabled on the device.
- Useful as an **Android-only fallback** when Wi-Fi Aware isn't available on a given
  device, per ChatGPT's suggested layering -- it is not a cross-platform path to iOS.

### BLE
- Already implemented (central/scan side) in this repo's `crates/mesh-transport-ble`
  via `btleplug`. Peripheral/advertising mode (needed so an Android device can be
  *found*, not just find others) needs native platform code -- already tracked as a
  known gap in this repo's own `README.md` Roadmap, not new information from this
  matrix.

---

## iOS / iPadOS

### Wi-Fi Aware
- **Availability**: Apple's own technote states plainly: *"iOS introduced support for
  Wi-Fi Aware in iOS 26. It's supported on iPhone 12 and later."* This is a hard
  hardware + OS floor -- there is no Wi-Fi Aware on iOS versions before 26, or on
  iPhones older than the 12, regardless of OS version installed.
- Apple explicitly frames it as *"an industry standard specification, opening up the
  possibility of communicating with non-Apple devices and accessories"* -- i.e. Apple
  itself says cross-platform interop is the intended point of this API, unlike the
  legacy peer-to-peer Wi-Fi mechanism below. This is *possibility*, not a claim of
  proven interop with any specific Android implementation -- still needs the Phase 2
  spike.
- **Entitlement**: requires `com.apple.developer.wifi-aware`, whose value is an array of
  capability strings (`Publish`, `Subscribe`) declaring which operations the app
  intends to use. NEEDS VERIFICATION: whether this entitlement is self-service
  (enabled directly in the developer account / Xcode Signing & Capabilities) or
  requires a special Apple approval process (like e.g. the Hotspot Helper entitlement
  documented on the same TN3111 page, which explicitly does require special Apple
  grant) -- the Wi-Fi Aware adoption page does not say either way in the sections
  fetched for this matrix. **Do not assume it's freely available -- confirm in Xcode's
  Signing & Capabilities editor / developer portal before planning around it.**
- **Service declaration**: every service name must be declared in `Info.plist` under a
  `WiFiAwareServices` dictionary, each entry marked `Publishable` and/or `Subscribable`
  (an empty dict for either enables that role). Service names must follow strict
  Bonjour-style naming (RFC 6763/6335: lowercase/digits/hyphen, `_name._tcp` or
  `_name._udp` suffix, ≤15 chars for the name component). **Declaring a service not
  matching these rules, or omitting both `Publishable`/`Subscribable`, crashes the
  app** -- this is an Apple-documented hard failure mode, not a soft error.
- **Data path**: publish/subscribe discovery yields `WAPublishableService`/
  `WASubscribableService` objects; actual data transfer goes through Network framework
  connections built on top of the discovered service. **Confirmed from Apple's own
  sample code** (`Building peer-to-peer apps`, `Connecting devices for peer-to-peer
  Wi-Fi` -- both fetched and read in full for this matrix, real API shown, not
  guessed):
  - Publisher: `NetworkListener(for: .wifiAware(.connecting(to: .someService, from:
    .allPairedDevices)), using: { ... }).run { connection in ... }`
  - Subscriber: `NetworkBrowser(for: .wifiAware(.connecting(to: .allPairedDevices,
    from: .someService)))`, then `browser.run { endpoints in ... }` to pick an
    endpoint, then `NetworkConnection(to: endpoint, using: { ... })`.
  - Data is sent/received as ordinary Network-framework messages/bytes over that
    connection (`connection.send(...)`, `for try await (event, _) in
    connection.messages`).
  - **Critical, previously-unconfirmed fact, now confirmed**: iOS Wi-Fi Aware requires
    devices to be explicitly **paired** first, via `DeviceDiscoveryUI`
    (`DevicePicker`/`DevicePairingView`) or `AccessorySetupKit` -- this is a
    user-facing UI flow (the person taps "+" and confirms pairing on both devices),
    not something an app can do silently/headlessly in the background. Only after
    pairing does a device appear in `WAPairedDevice.allDevices`, and only paired
    devices can be selected as the `from:`/`to:` target of a `NetworkListener`/
    `NetworkBrowser`. **This is a meaningful asymmetry vs. Android's model** (Android's
    Wi-Fi Aware publish/subscribe/connect flow, per the official sample fetched
    earlier, has no equivalent OS-level "pairing" concept -- a subscriber simply
    discovers and connects to any matching publisher in range). Any Phase 2 spike
    design must account for this: the iOS side cannot be a fully headless background
    responder the way the Android side can.
  - Apple's own sample-code page states outright: **"you can't run this sample in
    Simulator — you'll need to run it on physical devices."** This directly confirms
    (not just infers) that Phase 2/4 cannot be attempted in any simulator/emulator
    environment, including this development sandbox.
  - Performance/config knobs confirmed available: `WAPerformanceMode` (`.bulk` vs
    `.realtime`), `WAAccessCategory`/service class, and a `WAPerformanceReport`
    (signal strength 0.0-1.0, per-access-category transmit latency) obtainable via
    `connection.currentPath?.wifiAware`.
- **Background restrictions**: NEEDS VERIFICATION -- not covered in the pages fetched.

### Apple peer-to-peer Wi-Fi (legacy, iOS 7+)
- Available on **all** iOS/iPadOS/macOS/tvOS/visionOS devices, no special hardware
  floor, accessible via Network framework peer-to-peer parameters (not only Multipeer
  Connectivity -- Apple explicitly calls out that common misconception).
- **Apple's own words: "not documented for third-party use, so this mechanism only
  works between Apple devices."** This confirms ChatGPT's framing exactly: usable as an
  **iPhone-to-iPhone fallback**, never as the cross-platform (Android-interop) path.

### BLE
- Not covered in the pages fetched for this matrix (Core Bluetooth is well-established
  and not new information); central/peripheral roles both exist on iOS, with
  peripheral/background-advertising restrictions that are already a known, tracked gap
  in this repo (see `README.md` Roadmap) rather than something newly discovered here.

---

## What is CONFIRMED vs. what still NEEDS VERIFICATION

**Confirmed from official docs (no further research needed to know this much):**
- Wi-Fi Aware exists as a named, documented API on both platforms today.
- iOS's floor is hard: iOS/iPadOS 26+ AND iPhone 12+. There is no way around this for
  older hardware.
- Android's Wi-Fi Aware is chipset-dependent even above API 26 -- must runtime-detect,
  never assume from OS version.
- Apple's legacy peer-to-peer Wi-Fi is explicitly Apple-only for third-party apps --
  ruled out as a cross-platform path by Apple's own documentation, not by inference.
- Android Wi-Fi Aware's lightweight message API caps out around 255 bytes and is
  explicitly unreliable/unordered/possibly-duplicated by design -- only the negotiated
  network/socket data path is viable for real MeshTalk traffic.

**Explicitly NOT yet known -- do not treat as true until proven:**
- Whether an iPhone 12+ (iOS 26+) Wi-Fi Aware *subscriber* can actually discover and
  open a data path with an Android Wi-Fi Aware *publisher*, or vice versa. Both sides
  implementing "Wi-Fi Aware" does not by itself guarantee interop (different NAN
  protocol versions, vendor extensions, or subtly incompatible negotiated parameters
  are all realistic failure modes). **This is exactly Phase 2's job.** This is made
  more uncertain, not less, by the newly-confirmed fact that iOS requires an
  explicit `DeviceDiscoveryUI` pairing step with no stated Android equivalent --
  whether Android's publisher/subscriber model can even be paired with from an iOS
  `DevicePicker`/`DevicePairingView` flow at all (as opposed to only pairing with
  other Apple devices) is itself unconfirmed and must be tested, not assumed.
- Whether the `com.apple.developer.wifi-aware` entitlement requires a special Apple
  approval/request process, or is available to any enrolled developer account.
- Real-world throughput, range, connection-setup latency, and reconnect behavior for
  either platform's Wi-Fi Aware data path.
- Background execution behavior for either platform while a Wi-Fi Aware/Wi-Fi Direct
  session is active but the app isn't foregrounded.
- Whether any physical test devices (iPhone 12+ running iOS 26+; an Android device with
  actual Wi-Fi Aware hardware support, confirmed via the two-step runtime check above)
  are currently available to this project for Phase 2/4 testing -- **this blocks
  starting Phase 2 and must be answered before writing spike code**.

---

## Recommendation

Proceed to Phase 2 (minimal Wi-Fi Aware publish/subscribe spike, Android ⇄ iOS in both
directions, no mesh protocol, no encryption, just proving a data path opens and carries
a fixed test string) **only** once physical test hardware meeting both platforms' floors
above is confirmed available -- this cannot be verified in this development environment
(no physical iOS/Android hardware attached; iOS Simulator and Android emulators do not
implement real Wi-Fi Aware/BLE radios -- Apple's own sample-code page confirms this
explicitly for iOS: "you can't run this sample in Simulator"). Do not claim "routerless
support" anywhere in this project's documentation until Phase 2 (and, ultimately, Phase
4's hard no-router physical test) actually passes.

Draft (untested, not wired into any build) Phase 2 spike source, based only on the
confirmed API shapes documented above, lives under `spikes/routerless-transport/` --
see that directory's `README.md` for exactly what it is, what it deliberately doesn't
do yet, and the steps needed to turn it into a real, testable target on physical
hardware.
