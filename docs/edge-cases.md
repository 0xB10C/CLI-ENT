# Edge-case cookbook

Ready-to-run sequences for probing a Bitcoin node with `cli-ent`. These need no
new code — they are existing commands, mostly under `--handshake manual` so
nothing is sent automatically. Each entry notes Bitcoin Core's expected behaviour
so a deviation stands out. Exact disconnect/ignore behaviour drifts across
releases, so treat the notes as a starting point and record what you observe.

Run interactively, or save a block as a script and pass `--script`. Under
`--handshake manual` the session is driven entirely by what you type: send
`version` and `verack` yourself.

## Handshake / state-machine order

Start each with `--handshake manual` so nothing is automatic.

```
# message before version — Core disconnects
connect 127.0.0.1:18444 --v1
send ping nonce=1

# verack before version
send verack

# duplicate version
send version
send version

# feature negotiation in the wrong order — wtxidrelay must precede verack (BIP339)
send version
send verack
send wtxidrelay          # too late; Core ignores or disconnects by version

# never send verack — how long until the node gives up? (Core: ~60s)
send version
# ...wait and watch

# announce an ancient protocol version, then use a modern message
send version protocol=60000
send verack
send feefilter 1000      # feefilter is 70013+; does the node version-gate it?
```

## Volume / rate limiters

```
# pings as fast as possible — watch the node's backlog handling
spam ping

# find the rate where it starts pushing back
spam ping --rate 5000/s
stop

# inv for tx we'll never deliver — exercises in-flight tracking
craft inv --claim 1000 --actual 1000 --type tx
```

## Structural / allocation

```
craft inv --claim 60000 --actual 1          # above MAX_INV_SZ (50000) — Core disconnects
craft compactsize --value 5 --width 9       # non-minimal CompactSize
craft count-overflow inv                     # claims 2^64-1 items
craft big-field version user_agent 5000      # subversion over Core's 256 cap
craft long-locator --hashes 2000            # getheaders locator over the ~101 cap
craft bad-invtype --type 99                  # unknown inventory type
```

## Malformed frames

```
# v1
misbehave bad-checksum ping nonce=1          # Core ignores the message, keeps the peer
misbehave bad-magic ping nonce=1             # Core disconnects
misbehave wrong-length ping nonce=1 +10      # header length lies about the payload
misbehave oversize inv 4000001               # above Core's 4 MB message cap — disconnect
misbehave unknown-cmd frobnicate deadbeef    # unknown 12-byte command

# v2 (magic and checksum are gone; the length is encrypted)
misbehave corrupt ping nonce=1 1             # flip a length byte -> garbage length
misbehave corrupt ping nonce=1 20            # flip a body byte  -> auth failure
misbehave oversize inv 4000001               # padded contents
```

## Timing / connection

```
# slowloris — drip a version in one byte at a time
hold version <len> --drip 1 500ms

# truncated message, then walk away
hold inv 5
drop

# half-open: complete the handshake, stop reading, let the node's send buffer fill
pause-reads
# ...observe; then
resume-reads
```

## Consensus-content (poison samples)

```
send block sample:block-badmerkle     # accepted as a message, rejected on the merkle check
```

The `block-badmerkle` sample is `block1` with one byte of its merkle root flipped;
the node accepts the well-formed message and then rejects it on the merkle check —
a different code path from a malformed frame. Two further poison artifacts named in
the plan are not yet embedded and are good future additions: `block-dupetx` (the
CVE-2012-2459 duplicate-transaction mutation) and `tx-nonstandard` (a
structurally valid but policy-rejected transaction). Both are static hex that would
be generated once and checked in with a note on how they were produced.
