# cli-ent

**CLI Extended Node Talker** — an interactive Bitcoin P2P client. It connects to a
single peer over the v1 (plaintext) or v2 (BIP324) transport, performs the version
handshake, lets you send any P2P message via a small DSL, presets, or raw hex,
prints every message in and out, and can deliberately misbehave to observe how the
remote node reacts.

It is a debugging and exploration tool: well-behaved by default, misbehaviour
opt-in. One peer at a time, outbound only. No chain validation, mempool, or wallet.

## Build and run

The project builds with the provided Nix dev shell (which supplies the Rust
toolchain, a C compiler for `secp256k1`, and `bitcoind` for tests):

```sh
nix develop --command cargo build
nix develop            # or drop into the shell, then use cargo directly
```

Connect to a peer:

```sh
cli-ent 127.0.0.1:8333                    # mainnet, auto transport (v2, fall back to v1)
cli-ent 127.0.0.1:18444 --network regtest --transport v1
cli-ent                                   # start disconnected; use `connect` in the REPL
```

## The REPL

```
connect <host:port> [--v1|--v2|--auto]    connect to a peer
disconnect                                close the connection
status                                    peer, transport, handshake, negotiation, traffic
send <message> [field=value ...]          send a message (see below)
send <message> hex:<payload>              any variant, payload as hex
send raw <command> <hex>                  arbitrary 12-byte command + payload
preset [list | <name>]                    send a canned sequence
auto [list | on <name> | off <name>]      toggle the automatic responders
show [last | <seq>] [hex]                 annotated (default) or raw hex dump
misbehave <kind> ...                      malformed frames (§ below)
craft <kind> ...                          structural lies (bad CompactSize, etc.)
spam <message> [--rate <n>/s] [--count <n>]   cancellable repeating send
stop                                      end a running spam
hold <message> <n> [--drip <bytes> <interval>]   send a partial message / slowloris
release | drop                            release or discard held bytes
pause-reads | resume-reads                stop / resume draining the socket
wait <duration>                           (script only) pause
help [<command>]                          usage
quit                                      leave
```

Tab completion covers command, message, preset, and automation names. Ctrl-C
cancels the current line; Ctrl-D quits. Output is coloured on a TTY and plain when
piped or redirected.

## Sending messages

The field DSL covers the common messages; everything else goes through `hex:`.

```
send ping [nonce=<u64>]
send pong nonce=<u64>
send feefilter <sat/kvB>
send sendcmpct hb=<bool> version=<u64>
send inv|getdata|notfound tx=<txid>,… block=<hash>,… wtx=<wtxid>,… cmpct=<hash>,…
send getheaders|getblocks locator=<hash>[,…] [stop=<hash>]
send getcfilters|getcfheaders type=<u8> start=<u32> stop=<hash>
send getblocktxn block=<hash> idx=<n>[,…]
send version [user_agent=… protocol=… services=… height=… relay=… nonce=… timestamp=… addr_recv=ip:port addr_from=ip:port]
send tx <hex> | send tx sample
send block sample:genesis|block1|block-badmerkle | send block <hex>
send headers sample:block1|genesis
send cmpctblock sample:block1 | send blocktxn sample:block1 idx=<n>[,…]
```

Service flags are an integer (`0x…`/decimal) or `|`-joined names
(`NETWORK|WITNESS|…`).

## Transports

`--transport auto` (default) sends the v2 key and waits up to 3 seconds for the
peer's key; if the peer closes, resets, times out, or is detected as v1, it opens a
fresh connection and runs v1. `--transport v1` never probes; `--transport v2` never
falls back. `--magic <hex8>` overrides the network magic and is v1-only.

## Handshake control

`--handshake core` (default) mimics Bitcoin Core's negotiation; `minimal` sends only
version/verack; `manual` sends nothing (you drive it). `--verack manual` waits for
`send verack`; `--verack-delay <dur>` delays only the verack. `version` fields are
set with flags (`--user-agent`, `--protocol-version`, `--services`,
`--start-height`, `--relay`) or, in full, through `send version …`.

## Automations

Toggleable responders (`auto on|off <name>`):

| name          | trigger              | action                          | default |
|---------------|----------------------|---------------------------------|---------|
| `pong`        | ping                 | pong with the same nonce        | on      |
| `headers-empty` | getheaders/getblocks | empty headers / nothing       | on      |
| `serve`       | getdata for a sample | tx / block / notfound           | on      |
| `getdata`     | inv                  | getdata for every item          | off     |

## Misbehaviour

**v1:** `misbehave bad-checksum <msg>`, `bad-magic <msg>`, `wrong-length <msg> +N|-N`,
`oversize <msg> <bytes>`, `unknown-cmd <cmd> [<hex>]`.
**v2:** `misbehave corrupt <msg> <offset>`, `oversize <msg> <bytes>`. Inapplicable
kinds report "not applicable".

**Structural lies:** `craft inv --claim <N> --actual <M> [--type tx|block|wtx]`,
`craft compactsize --value <V> --width <1|3|5|9>`, `craft count-overflow <msg>`,
`craft big-field version user_agent <bytes>`, `craft long-locator [--hashes <N>]`,
`craft bad-invtype --type <u32>`.

See [`docs/edge-cases.md`](docs/edge-cases.md) for a cookbook of ready-to-run
sequences and Core's expected behaviour.

## Scripting

```sh
cli-ent 127.0.0.1:18444 --network regtest \
  --script tests/inv-tx.txt --script-delay 200ms --exit-after-script
```

Lines are REPL commands; blank lines and `#` comments are ignored; `wait <dur>`
pauses. The script starts once the handshake is ready (or, under `--handshake
manual`, as soon as the transport is up). `--exit-after-script` exits 0 on success,
1 if the peer disconnected first. `--log <path>` mirrors every line to a file with
absolute timestamps and colour stripped.

## Tests

Unit tests run with `cargo test`. The integration tests spawn a real regtest
`bitcoind` and assert via RPC; they are `#[ignore]` by default:

```sh
export BITCOIND_EXE="$(command -v bitcoind)"    # or rely on the download feature
nix develop --command cargo test --test regtest -- --ignored
```

## License

MIT.
