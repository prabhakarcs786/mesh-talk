// Milestone 4A, Phase 2 -- DRAFT, UNTESTED. See ../README.md before using this file.
//
// Minimal Wi-Fi Aware publish/subscribe/connect spike for iOS/iPadOS 26+, iPhone 12+
// only. Proves nothing about MeshTalk's mesh protocol -- it exchanges exactly one
// fixed literal string, `MESHTALK_ROUTERLESS_TEST_V1`, over a Wi-Fi Aware data path,
// per Milestone 4A Phase 2's scope.
//
// Every API used here is taken directly from Apple's own sample code and
// documentation (not guessed):
//   - "Building peer-to-peer apps": https://developer.apple.com/documentation/wifiaware/building-peer-to-peer-apps
//   - "Connecting devices for peer-to-peer Wi-Fi": https://developer.apple.com/documentation/wifiaware/connecting-paired-devices
//   - "Adopting Wi-Fi Aware": https://developer.apple.com/documentation/WiFiAware/Adopting-Wi-Fi-Aware
//
// Prerequisites this file assumes but does NOT set up (see ../README.md):
//   1. A separate Xcode target (NOT the main MeshTalk app) with the
//      `com.apple.developer.wifi-aware` entitlement, capability array `["Publish",
//      "Subscribe"]`.
//   2. That target's Info.plist declares:
//        WiFiAwareServices -> "_meshtalk-rlt._tcp" -> Publishable: {}, Subscribable: {}
//   3. Physical iPhone 12+ running iOS 26+. Apple's own sample page states you
//      "can't run this sample in Simulator". This file has never been compiled or
//      run -- treat every line as unverified until it has been.
//   4. A user-driven pairing step BEFORE any of the code below can target a specific
//      peer -- Apple's sample confirms Wi-Fi Aware connections only target *paired*
//      devices (`WAPairedDevice`), paired via `DeviceDiscoveryUI`'s `DevicePicker`
//      (subscriber side) / `DevicePairingView` (publisher side), or `AccessorySetupKit`.
//      That SwiftUI pairing flow is NOT implemented below (only sketched in comments)
//      because no full code sample for it was fetched/verified for this draft -- do
//      not guess it; read
//      https://developer.apple.com/documentation/DeviceDiscoveryUI before implementing.

import Foundation
import Network
import WiFiAware // iOS/iPadOS 26+ only -- guard all use sites with @available.
import os.log

private let logger = Logger(subsystem: "com.meshtalk.routerless-spike", category: "wifi-aware")

/// Must exactly match the `WiFiAwareServices` key declared in the spike target's
/// Info.plist (see prerequisite 2 above) -- Apple's docs warn a mismatch here is a
/// crash, not a soft failure.
private let meshTalkSpikeServiceName = "_meshtalk-rlt._tcp"

/// Fixed test payload for Milestone 4A Phase 2 -- deliberately not mesh-protocol
/// content, not encrypted, just proving a data path carries bytes at all.
private let testPayload = "MESHTALK_ROUTERLESS_TEST_V1".data(using: .utf8)!

@available(iOS 26.0, *)
extension WAPublishableService {
    static var meshTalkSpikeService: WAPublishableService {
        allServices[meshTalkSpikeServiceName]!
    }
}

@available(iOS 26.0, *)
extension WASubscribableService {
    static var meshTalkSpikeService: WASubscribableService {
        allServices[meshTalkSpikeServiceName]!
    }
}

/// One exchanged event -- intentionally trivial (Phase 2 scope: prove transport only).
@available(iOS 26.0, *)
enum SpikeEvent: Codable, Sendable {
    case testString(String)
}

@available(iOS 26.0, *)
typealias SpikeConnection = NetworkConnection<Coder<SpikeEvent, SpikeEvent, NetworkJSONCoder>>

/// Publisher role: advertises the spike service and echoes the test payload back to
/// whoever connects. Corresponds to Apple's "Create a listener to publish" sample.
///
/// TODO (blocked on hardware + pairing UI, see file header): before this can target
/// real paired devices, the app needs a `DevicePairingView(.wifiAware(.connecting(to:
/// .meshTalkSpikeService, from: .userSpecifiedDevices)))` flow (or similar -- verify
/// against https://developer.apple.com/documentation/DeviceDiscoveryUI/DevicePairingView
/// before writing it) so `WAPairedDevice.allDevices` actually contains a peer to allow
/// connections from. `.allPairedDevices` below is a placeholder for "whichever peers
/// got paired via that not-yet-written flow."
@available(iOS 26.0, *)
final class SpikePublisher {
    private var listener: NetworkListener<SpikeEvent, SpikeEvent, NetworkJSONCoder>?
    private var connections: [SpikeConnection] = []

    func start() async throws {
        let listener = try await NetworkListener(
            for: .wifiAware(.connecting(to: .meshTalkSpikeService, from: .allPairedDevices)),
            using: {
                Coder(receiving: SpikeEvent.self, sending: SpikeEvent.self, using: NetworkJSONCoder()) {
                    TLS()
                }
            }
        )
        .onStateUpdate { listener, state in
            logger.info("publisher listener state: \(String(describing: state))")
        }
        .run { [weak self] connection in
            logger.info("publisher: incoming connection")
            self?.handle(connection)
        }
        self.listener = listener
    }

    private func handle(_ connection: SpikeConnection) {
        connection.onStateUpdate { connection, state in
            logger.info("publisher connection state: \(String(describing: state))")
        }
        connections.append(connection)
        Task {
            do {
                for try await (event, _) in connection.messages {
                    if case let .testString(received) = event {
                        logger.info("publisher received: \(received, privacy: .public)")
                        // Echo it straight back so the subscriber can confirm a
                        // full round trip, not just one-way delivery.
                        try await connection.send(.testString(received))
                    }
                }
            } catch {
                logger.error("publisher connection error: \(error, privacy: .public)")
            }
        }
    }
}

/// Subscriber role: discovers the publisher, connects, sends the fixed test payload,
/// and waits for the echoed reply. Corresponds to Apple's "Create a browser to
/// subscribe" + "Make a connection" samples.
///
/// Same pairing-flow caveat as `SpikePublisher` applies -- see its doc comment.
@available(iOS 26.0, *)
final class SpikeSubscriber {
    func runOnce() async throws -> String {
        let browser = NetworkBrowser(
            for: .wifiAware(.connecting(to: .allPairedDevices, from: .meshTalkSpikeService))
        )
        .onStateUpdate { browser, state in
            logger.info("subscriber browser state: \(String(describing: state))")
        }

        let endpoint = try await browser.run { waEndpoints in
            logger.info("subscriber discovered \(waEndpoints.count) endpoint(s)")
            if let first = waEndpoints.first {
                return .finish(first)
            }
            return .continue
        }

        let connection = SpikeConnection(
            to: endpoint,
            using: {
                Coder(receiving: SpikeEvent.self, sending: SpikeEvent.self, using: NetworkJSONCoder()) {
                    TLS()
                }
            }
        )
        connection.onStateUpdate { connection, state in
            logger.info("subscriber connection state: \(String(describing: state))")
        }

        try await connection.send(.testString(String(data: testPayload, encoding: .utf8)!))

        for try await (event, _) in connection.messages {
            if case let .testString(echoed) = event {
                return echoed
            }
        }
        throw NSError(domain: "MeshTalkRoutlerlessSpike", code: 1, userInfo: [NSLocalizedDescriptionKey: "connection closed before a reply arrived"])
    }
}
