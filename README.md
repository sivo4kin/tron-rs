# tron-rs

A production-oriented **Tron full node in Rust**, built greenfield against java-tron as the
single source of truth, with **differential verification on live-chain data** at every step.
See [`SPEC.md`](./SPEC.md) for the full design, testing strategy, and phased roadmap.

## Status

| Phase | State |
|---|---|
| P0 — Scaffold (workspace, proto codegen, primitives, booting node) | **done** |
| P1 — Chain & state | **in progress** (6 iterations landed, see below) |
| P2 — TVM · P3 — Networking · P4 — APIs · P5 — SR | not started |

### Verified parity so far (against real java-tron blocks)

The `verify/` harness captures raw blocks from live networks over gRPC (using our own
generated `Wallet` client) and asserts byte-equality with what java-tron computed:

- **`txTrieRoot`** recomputed from raw transaction bytes matches the block header on all
  fixtures — including a **683-tx mainnet block** (also proves prost serialization is
  byte-identical to java-tron's protobuf).
- **Block id layout** (8-byte BE height ‖ `sha256(headerRaw)[8..]`) and parent linkage.
- **Witness signature** — the block producer's address recovered from the real 65-byte
  header signature equals `witness_address` (sha256 → secp256k1-recover → keccak address).
- **718 live transaction signatures** recover; Nile txs match `owner_address` 100%
  (mainnet's delegated remainder is permission-based signing — expected).

- **257 real TransferContract txs** replayed through the executor (real `Any`
  packing) with value conservation on every one.
- **Real accounts** re-encode byte-identically (vendored-proto currency guard) and
  round-trip unchanged through the state store.
- **Full structural block validation** passes on every fixture (txTrieRoot,
  parent link, witness signature) and rejects tampered blocks.

Component layers: **13 contract types routed through the executor** across six
actuator modules (transfer, TRC10, freeze/unfreeze v2, vote, proposal x3,
account x3, witness x2), each with a java-tron-mirrored test table and
conservation invariants; RocksDB-backed storage behind the `rocksdb` feature.
**142 tests green.**

## Layout

```
crates/
  types      Address (21-byte 0x41, Base58Check/hex), H256, Sun
  crypto     sha256/keccak256, secp256k1 sign/recover, Tron address derivation
  proto      prost + tonic codegen from vendored java-tron .proto (wire + gRPC)
  storage    KvStore trait + in-memory impl (RocksDB behind a feature, P1)
  state      typed world-state stores (accounts = protocol.Account values, properties)
  chain      tx id / merkle root / block id / witness+tx signature recovery
  actuators  system-contract executors (TransferActuator done)
  vm         TVM energy meter (revm adaptation is the P2 spike)
  consensus  DPoS reference params + slot math (validation in P3)
  p2p        discovery/channel scaffolding (P3)
  rpc        HTTP/gRPC/JSON-RPC scaffolding (P4)
  node       config + service supervisor with graceful shutdown
verify/      differential harness: fixture capture bin + parity tests
```

## Commands

```bash
cargo test --workspace          # full suite (offline; fixtures are committed)
cargo run -p tron-node          # boot the node skeleton (Ctrl-C to stop)

# Capture fresh differential fixtures from a live network:
cargo run -p tron-verify --bin capture -- 69582212            # Nile (default endpoint)
cargo run -p tron-verify --bin capture -- \
  --endpoint http://grpc.trongrid.io:50051 84861706           # mainnet
```

## Next (P1 remainder)

Maintenance-cycle logic (vote counting -> active witness set), the differential
**state diff** vs a java-tron reference node (SPEC section 7) as the D3 parity
gate, remaining actuator families (exchange, market, resource delegation), and
genesis-state initialization for from-scratch replay.
