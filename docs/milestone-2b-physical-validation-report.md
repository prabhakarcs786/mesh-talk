# Milestone 2B — Physical-Device Validation Report

**Status of this document:** template, not yet executed. No physical iPhone or
Android device (and no Android emulator) was available in the environment this
was prepared in. Fill this in once real devices are connected.

## Purpose and ground rules

This report exists to answer exactly one question, honestly:

> Do the actual, real, compiled iOS and Android apps exchange DirectV1 messages
> correctly under realistic conditions — not just Rust automated tests?

Rules for this milestone, while this report is being filled in:

- **No new features.** Do not implement QR/safety-number verification, DTN/
  store-and-forward, routing changes, call changes, Settings UI, or persistent
  replay protection during this milestone. Bug fixes to make an existing,
  already-claimed behavior actually work correctly are in scope; new
  capabilities are not.
- **PASS means it was observed working on a real device**, not "the code looks
  like it should work" and not "the equivalent Rust test passes." Rust tests
  already prove the protocol logic in isolation (see `crates/mesh-core`,
  `crates/mesh-mobile` test suites, currently 87 + 12 passing). This report is
  specifically about what only a real device build can prove: real sockets,
  real OS lifecycle (backgrounding, lock screen, network changes), real
  cross-platform serialization, real discovery over real Wi-Fi.
- **Every row must be marked PASS, FAIL, or BLOCKED** — never left blank, never
  marked "probably fine." BLOCKED means the test could not be attempted (e.g.
  no second device of that platform available) — distinct from FAIL.
- Record exact NodeIds using **short form only** (the 6-hex-char short id shown
  in the UI / `short_id()`), never the full 32-byte hex NodeId, and never any
  identity seed, private key, or session key, in this report or in any
  attached logs.

## Known, already-accepted gaps (do not treat these as new bugs)

- **No durable store-and-forward yet.** An offline recipient will never
  actually receive a message sent while they were offline (no relay queue
  exists yet) — Test E below only checks that *sending*/*encrypting* succeeds
  despite the recipient being offline, not that delivery eventually happens.
- **"Secure" in the UI means cryptographic key ownership was verified, not
  human identity.** `ContactVerification` will read `Unverified` for every
  contact throughout this milestone — that is expected until Milestone 2C
  (QR/safety-number verification) exists.

Note: persistent replay protection (Milestone 2D) is now implemented
(`mesh_core::ReplayStore`, SQLite-backed, keyed by `(sender, message_id)`) and
proven at both the `mesh-core` and `mesh-mobile` layers by automated tests
(`replayed_message_is_rejected_after_restart`,
`relay_does_not_forward_a_replayed_message_after_its_own_restart`,
`replayed_packet_is_rejected_after_a_mesh_client_restart`, among others) — Test
F below should now PASS on real devices too, not just document a known gap.

## Device inventory

| Role | Device model | OS version | App build (commit hash) | NodeId (short) |
|---|---|---|---|---|
| Device A | | | | |
| Device B | | | | |
| Device C (if available) | | | | |
| Device D (if available) | | | | |

## Test matrix — which platform pairs to run

| Sender | Receiver | Priority | Status |
|---|---|---|---|
| iPhone | iPhone | High | BLOCKED / attempted |
| Android | Android | High | BLOCKED / attempted |
| iPhone | Android | **Highest** | BLOCKED / attempted |
| Android | iPhone | **Highest** | BLOCKED / attempted |

Cross-platform pairs matter most: serialization and protocol assumptions that
happen to work when both sides run the same platform's build are the ones most
likely to break across platforms.

---

## Test A — Identity stability across a full app kill

**Procedure:**
1. Launch the app fresh on both devices (first-ever launch or after a clean
   reinstall). Record each NodeId (short form).
2. Fully kill both apps (swipe away / force-stop, not just backgrounding).
3. Relaunch both apps.
4. Compare NodeIds to step 1.

**Requirement:** both NodeIds are unchanged after the kill+relaunch.

| Field | Value |
|---|---|
| Alice NodeId before | |
| Alice NodeId after | |
| Bob NodeId before | |
| Bob NodeId after | |
| Result | PASS / FAIL / BLOCKED |
| Notes | |

---

## Test B — Discovery and contact persistence

**Procedure:**
1. With both apps on the same Wi-Fi network, wait for discovery.
2. Confirm each device shows the other as a discovered/contact.
3. Restart both apps (full kill + relaunch).
4. Confirm both still show each other as a known contact *without* needing to
   wait for rediscovery first.

**Requirement:**
- Alice discovers Bob, Bob discovers Alice.
- X25519 binding is valid (app does not show any "invalid identity" state).
- Contact is stored on both sides.
- Contact state reads "secure cryptographic identity" (not yet human-verified
  — see known gaps above).
- After restart, both contacts are still present (loaded from the persistent
  `ContactStore`), not just re-populated by a fresh discovery broadcast.

| Field | Value |
|---|---|
| Result (initial discovery) | PASS / FAIL / BLOCKED |
| Result (contact survives restart) | PASS / FAIL / BLOCKED |
| Notes | |

---

## Test C — DirectV1 text message, both directions

**Procedure:** send this exact marker string in both directions:

```
MESHTALK_E2EE_TEST_482961
```

Inspect available protocol logs (stderr / Xcode console / logcat if wired up)
for the `encryption_mode` used. If packet capture is feasible (e.g. a
Wi-Fi packet capture tool on the same network, or a debug build with a raw
capture hook), inspect the actual bytes for the marker string.

**Requirement:**

| Check | Result |
|---|---|
| Alice → Bob delivered and shown correctly in UI | PASS / FAIL / BLOCKED |
| Bob → Alice delivered and shown correctly in UI | PASS / FAIL / BLOCKED |
| Logged `encryption_mode` = DirectV1 for both directions | PASS / FAIL / BLOCKED |
| Marker string absent from any captured/logged raw wire bytes | PASS / FAIL / BLOCKED |

Notes:

---

## Test D — DirectV1 file transfer

**Procedure:** create a small file (e.g. a `.txt`) containing:

```
MESHTALK_PRIVATE_FILE_712643
```

Send it as an attachment in one direction (repeat the other direction if time
allows).

**Requirement:**

| Check | Result |
|---|---|
| File received and content byte-for-byte correct | PASS / FAIL / BLOCKED |
| Marker string absent from any captured/logged raw wire bytes | PASS / FAIL / BLOCKED |

Notes:

---

## Test E — Offline recipient, encryption still succeeds

**Procedure:**
1. Alice and Bob discover each other normally.
2. Fully close Bob's app.
3. Restart Alice's app.
4. With Bob still offline, have Alice send Bob a message.

**Requirement:** sending/encryption succeeds using Bob's persisted identity —
the UI must NOT show "Secure identity unavailable" or any fail-closed message,
since Bob's `PublicIdentity` should already be in Alice's persisted contact
cache. Actual delivery to Bob is **not** required to succeed (no durable
store-and-forward exists yet — see known gaps above) — only that sending
itself was accepted, not refused for lack of a known identity.

| Field | Value |
|---|---|
| Result | PASS / FAIL / BLOCKED |
| Notes | |

---

## Test F — Receiver restart, delayed delivery

**Procedure:**
1. Capture or otherwise hold a DirectV1 packet sent to Bob (e.g. Bob offline
   when it's sent, or a deliberately delayed redelivery if your test setup
   allows it).
2. Fully kill Bob's app.
3. Restart Bob's app.
4. Deliver the held packet to Bob (however is feasible given your test setup
   — e.g. having Alice resend after Bob comes back online is an acceptable
   substitute for literal packet redelivery if raw capture/replay isn't
   practical on real hardware).

**Requirement:** Bob successfully decrypts and displays the message after
restarting. This is the mobile-stack equivalent of the already-passing Rust
tests `persisted_offline_contact_survives_restart_and_decrypts_after_capture`
(mesh-mobile) and `replayed_message_is_rejected_after_restart`/
`replayed_packet_is_rejected_after_a_mesh_client_restart` (replay case) — this
test is about confirming the real mobile app behaves the same way, not
re-deriving the proof from scratch. Also confirm: replaying the *same* packet
a second time after Bob's restart is correctly rejected (not delivered twice).

| Field | Value |
|---|---|
| Result | PASS / FAIL / BLOCKED |
| Notes | |

---

## Test G — Identity replacement / IdentityChanged detection

**Procedure:**
1. Alice and Bob are already contacts (from Test B).
2. Reset/reinstall Bob's app so it generates a brand-new identity (new NodeId
   or at least new keys — check `IdentityKeychain`/`IdentityStore` reset, or
   simply clear app data).
3. Let Bob re-advertise via discovery so Alice sees the new identity.
4. Restart Alice's app.

**Requirement:**
- Alice's app surfaces an explicit "Identity changed" warning for Bob — it
  must **not** silently start showing Bob as trusted/secure with the new
  identity without any warning.
- After restarting Alice, the warning is still present (persisted, not lost
  on restart) until explicitly acknowledged.

| Field | Value |
|---|---|
| Result (warning shown) | PASS / FAIL / BLOCKED |
| Result (warning survives Alice's restart) | PASS / FAIL / BLOCKED |
| Notes | |

---

## Test H — Fail-closed on missing identity

**Procedure:**
1. Remove/clear Bob from Alice's contacts (e.g. via a fresh install, or a
   `resetContacts()`-triggering action if exposed in a debug menu).
2. With no stored identity for Bob, attempt to send Bob a private text message
   (e.g. by manually addressing a stale conversation, if the UI allows it).

**Requirement:** the app refuses to send and reports something equivalent to
`CannotEncryptForRecipient` / "Secure identity unavailable." It must **not**
fall back to `ChannelV1` (the shared-passphrase channel encryption) and must
**not** send anything in plaintext.

| Field | Value |
|---|---|
| Result | PASS / FAIL / BLOCKED |
| Notes | |

---

## Test I — Malformed / invalid discovery announcements (if feasible)

If you have a way to send a hand-crafted or corrupted discovery announcement
(e.g. a modified `mesh-cli`/test tool, or a second device running a
deliberately patched build), confirm:

| Check | Result |
|---|---|
| Announcement with invalid X25519 binding is rejected, not stored | PASS / FAIL / BLOCKED |
| Announcement does not silently "update" an existing trusted contact | PASS / FAIL / BLOCKED |

If not feasible with available hardware, mark **BLOCKED** — this scenario is
already covered by the Rust automated test suite
(`invalid_x25519_binding_in_header_is_rejected` and related tests in
`crates/mesh-transport-udp`/`crates/mesh-core`), so a BLOCKED result here is
acceptable and does not block sign-off.

---

## App-lifecycle spot checks (especially iOS)

Run each of these at least once and note anything unexpected — these are
exactly the kind of bugs that don't show up in Rust unit tests:

| Scenario | Observed behavior | Result |
|---|---|---|
| Foreground → send message | | PASS / FAIL |
| Background → foreground → send message | | PASS / FAIL |
| Screen locked → unlock → send message | | PASS / FAIL |
| Airplane mode / Wi-Fi off → on → rediscover | | PASS / FAIL |
| Switch Wi-Fi networks → rediscover | | PASS / FAIL |
| Force-kill → reopen | | PASS / FAIL |

Watch specifically for: lost contacts, duplicate contacts, a new NodeId
unexpectedly generated, discovery silently stopping, or old sockets/ports
lingering incorrectly.

---

## Diagnostic logging reference

Both `mesh-core` and `mesh-mobile` log (via the `log` crate facade, written to
stderr by a minimal logger installed in `MeshClient::new`) at points including:
send attempts, flood/relay fan-out, every `handle_incoming` decision (rejected/
duplicate/expired/decrypted), and contact-cache mutations (discovered/
identity-changed/acknowledged/reset). Each line includes only: `message_id`
(short prefix), short sender/recipient NodeIds, protocol version, encryption
mode, message type, and outcome — **never plaintext, private keys, identity
seeds, or session keys.** iOS: visible in the Xcode console when launched via
Xcode/`simctl launch --console`. Android: native stderr is not currently
bridged to `logcat` — treat this as a known instrumentation gap if Android-side
log visibility is needed for diagnosing a specific failure.

---

## Sign-off

| | |
|---|---|
| Tester | |
| Date | |
| Overall verdict | Milestone 2B certified / not yet certified |
| Blocking issues found | |
| Follow-up items filed | |

### The no-router checkpoint (separate from this report, do after 2C/2D)

Once this report is fully PASS (or all non-PASS rows are explicitly triaged),
the next major checkpoint — before continuing further up the security roadmap
— is a physical test with:

- Cellular data off on both devices
- Internet off
- No shared Wi-Fi router / access point

If Alice and Bob cannot communicate at all under those conditions, transport
(Wi-Fi Direct / Wi-Fi Aware / BLE) becomes the immediate priority over any
further security milestone, since MeshTalk's core premise is functioning with
no existing infrastructure.
