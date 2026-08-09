import Foundation

let alice = try! MeshClient(
    displayName: "alice",
    listenAddr: "0.0.0.0:9301",
    peerAddrs: ["127.0.0.1:9302"],
    channelPassphrase: "file-demo",
    ttl: 8
)
let bob = try! MeshClient(
    displayName: "bob",
    listenAddr: "0.0.0.0:9302",
    peerAddrs: ["127.0.0.1:9301"],
    channelPassphrase: "file-demo",
    ttl: 8
)

Thread.sleep(forTimeInterval: 0.3)

// Simulate a ~3MB "image" -- realistic size for an actual phone photo (thousands of
// chunks at CHUNK_SIZE=1KB), big enough to reproduce burst-related UDP packet loss if
// chunk sends aren't paced and receive buffers aren't sized for it.
var imageBytes = Data(count: 3_000_000)
for i in 0..<imageBytes.count {
    imageBytes[i] = UInt8((i * 37 + 11) % 256)
}

let sent = alice.sendFile(data: imageBytes, fileName: "photo.jpg", mimeType: "image/jpeg", kind: .image)
print("alice.sendFile ->", sent)

var received: ReceivedMessage? = nil
for _ in 0..<300 {
    if let msg = bob.pollMessage() {
        received = msg
        break
    }
    Thread.sleep(forTimeInterval: 0.2)
}

guard let msg = received, let attachment = msg.attachment else {
    print("FAILED: bob did not receive a file attachment")
    exit(1)
}

print("bob received attachment: name=\(attachment.name) mime=\(attachment.mime) kind=\(attachment.kind) bytes=\(attachment.data.count)")

guard attachment.data == imageBytes else {
    print("FAILED: reassembled bytes do not match the original (count: \(attachment.data.count) vs \(imageBytes.count))")
    exit(1)
}
print("SUCCESS: reassembled \(attachment.data.count) bytes match exactly")

// Also confirm a normal text message still works after a file transfer.
let textSent = bob.send(text: "got your photo!")
print("bob.send(text) ->", textSent)
var textReceived: ReceivedMessage? = nil
for _ in 0..<25 {
    if let msg = alice.pollMessage(), msg.text != nil {
        textReceived = msg
        break
    }
    Thread.sleep(forTimeInterval: 0.2)
}
guard let textMsg = textReceived, let text = textMsg.text else {
    print("FAILED: alice did not receive the follow-up text message")
    exit(1)
}
print("alice received text: \(text)")
