import Foundation

let alice = try! MeshClient(
    displayName: "alice",
    listenAddr: "127.0.0.1:9101",
    peerAddrs: ["127.0.0.1:9102"],
    channelPassphrase: "swift-demo",
    ttl: 8
)
let bob = try! MeshClient(
    displayName: "bob",
    listenAddr: "127.0.0.1:9102",
    peerAddrs: ["127.0.0.1:9101"],
    channelPassphrase: "swift-demo",
    ttl: 8
)

print("alice node id:", alice.nodeId())
print("bob node id:", bob.nodeId())

Thread.sleep(forTimeInterval: 0.3)

let sent = alice.send(text: "hello from swift!")
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
