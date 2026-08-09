import Foundation

let alice = try! MeshClient(
    displayName: "alice",
    listenAddr: "0.0.0.0:9201",
    peerAddrs: [],
    channelPassphrase: "discovery-demo",
    ttl: 8
)
let bob = try! MeshClient(
    displayName: "bob",
    listenAddr: "0.0.0.0:9202",
    peerAddrs: [],
    channelPassphrase: "discovery-demo",
    ttl: 8
)

print("alice node id:", alice.nodeId())
print("bob node id:", bob.nodeId())

try! alice.startDiscovery()
try! bob.startDiscovery()

var aliceFoundBob: DiscoveredPeer? = nil
var bobFoundAlice: DiscoveredPeer? = nil

for _ in 0..<30 {
    if aliceFoundBob == nil {
        aliceFoundBob = alice.discoveredPeers().first
    }
    if bobFoundAlice == nil {
        bobFoundAlice = bob.discoveredPeers().first
    }
    if aliceFoundBob != nil && bobFoundAlice != nil {
        break
    }
    Thread.sleep(forTimeInterval: 0.5)
}

guard let aliceSees = aliceFoundBob, let bobSees = bobFoundAlice else {
    print("FAILED: discovery did not find the other peer in time")
    exit(1)
}

print("alice discovered:", aliceSees.displayName, aliceSees.address, "code:", aliceSees.pairingCode)
print("bob discovered:", bobSees.displayName, bobSees.address, "code:", bobSees.pairingCode)

guard aliceSees.pairingCode == bobSees.pairingCode else {
    print("FAILED: pairing codes do not match (\(aliceSees.pairingCode) vs \(bobSees.pairingCode))")
    exit(1)
}
print("pairing codes match:", aliceSees.pairingCode)

// One-tap connect: no manual IP entry needed, just use the discovered address.
// aliceSees = what alice discovered about bob; bobSees = what bob discovered about alice.
alice.addPeer(address: aliceSees.address)
bob.addPeer(address: bobSees.address)

Thread.sleep(forTimeInterval: 0.3)
let sent = alice.send(text: "hello via auto-discovery!")
print("alice.send ->", sent)

var received: ReceivedMessage? = nil
for _ in 0..<25 {
    if let msg = bob.pollMessage() {
        received = msg
        break
    }
    Thread.sleep(forTimeInterval: 0.2)
}

if let msg = received {
    print("bob received: [\(msg.senderId)] \(msg.text)")
} else {
    print("FAILED: bob did not receive the message")
    exit(1)
}
