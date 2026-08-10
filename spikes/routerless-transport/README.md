# Routerless transport spike (Milestone 4A, Phase 2) -- DRAFT, UNTESTED

**Status: draft source only. Not wired into any buildable Xcode/Gradle target. Not
run, not tested, not verified on any hardware or simulator.**

## Why this exists, and why it's not "done"

`docs/routerless-transport-capability-matrix.md` (Phase 1) concluded that proving
cross-platform Wi-Fi Aware interop (Phase 2) and a hard no-router physical test
(Phase 4) both require real hardware this development environment does not have:

- an iPhone 12 or later, running iOS/iPadOS 26+ (Apple's Wi-Fi Aware framework has no
  Simulator support at all -- Apple's own sample-code page says so explicitly), and
- an Android device with Wi-Fi Aware hardware actually present and enabled (chipset-
  dependent even above API 26 -- must be confirmed at runtime, not assumed).

Rather than either (a) silently skipping Phase 2, or (b) writing untested code
straight into the real `ios/MeshTalk.xcodeproj` / `android/app` projects where a
mistake could destabilize the builds that were just gotten working end-to-end this
session, this directory holds **draft source files only**, isolated from both real
app projects, based strictly on API shapes confirmed from official Apple/Android
sample code and documentation (cited inline) -- never guessed.

## What's here

- `ios/WiFiAwareSpike.swift` -- draft Swift, using the real `NetworkListener`/
  `NetworkBrowser`/`NetworkConnection`/`.wifiAware(...)` APIs from Apple's
  "Building peer-to-peer apps" sample and "Connecting devices for peer-to-peer Wi-Fi"
  article. Requires a **new**, separate Xcode target (not the main `MeshTalk` target)
  with the `com.apple.developer.wifi-aware` entitlement and a `WiFiAwareServices`
  `Info.plist` entry for `_meshtalk-rlt._tcp` -- neither exists yet in this repo.
- `android/WifiAwareSpike.kt` -- draft Kotlin, using the real `WifiAwareManager`/
  `PublishConfig`/`SubscribeConfig`/`DiscoverySessionCallback`/`WifiAwareNetworkSpecifier`
  APIs and the exact 8-step publish→subscribe→message→socket connection recipe from
  Android's official "Wi-Fi Aware overview" guide (fetched and read in full for this
  spike). Requires the manifest permissions listed in the capability matrix, none of
  which are currently declared in `android/app/src/main/AndroidManifest.xml`. Unlike
  the iOS side, Android's model needed no OS-level "pairing" step to write against --
  it publishes/subscribes/connects directly.

Both exchange nothing but a fixed literal string, `MESHTALK_ROUTERLESS_TEST_V1`, per
Phase 2's own scope ("No mesh protocol initially. No encryption integration
initially. Just prove transport.").

## What this deliberately does NOT do

- Does not touch `mesh-core`, the `Transport` trait, or either real mobile app target.
- Does not attempt BLE, Wi-Fi Direct, or Apple peer-to-peer Wi-Fi fallbacks yet --
  Wi-Fi Aware first, per the priority order in the capability matrix.
- Does not implement the iOS `DeviceDiscoveryUI` pairing flow (`DevicePicker`/
  `DevicePairingView`) that Apple's own sample confirms is a *hard prerequisite*
  before any Wi-Fi Aware `NetworkListener`/`NetworkBrowser` can target a peer -- that
  UI is stubbed out with a `TODO` and a comment pointing at the real APIs
  (`WAPairedDevice.allDevices`, `DevicePicker`, `DevicePairingView`) rather than
  guessed, since no sample of that exact flow was fetched in full for this spike.
- Does not claim to compile, run, or pass. Someone with the physical hardware and an
  Apple Developer account (to confirm/request the `com.apple.developer.wifi-aware`
  entitlement -- see the capability matrix's open question on whether that needs
  special approval) needs to:
  1. Add a new minimal Xcode target (e.g. via `project.yml` + `xcodegen generate`,
     or directly in Xcode) with the entitlement + `Info.plist` service declaration,
     and drop `ios/WiFiAwareSpike.swift` into it.
  2. Add the Wi-Fi Aware manifest permissions to a throwaway Android module (or a
     debug-only screen in the existing app, gated so it can never ship), and drop
     `android/WifiAwareSpike.kt` in.
  3. Actually run the four PASS/FAIL/BLOCKED combinations Phase 2 calls for (Android→
     Android, iPhone→iPhone, Android→iPhone, iPhone→Android) and record real results.

## Recording results

Once real testing happens, record outcomes back in
`docs/routerless-transport-capability-matrix.md` (a new "Phase 2 results" section) --
PASS / FAIL / BLOCKED per direction, plus whatever setup time/throughput/range numbers
were observed. Only after that should Phase 3 (a real `Transport` adapter) begin.
