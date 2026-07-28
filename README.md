# tron-rs

A Tron full node in Rust, built greenfield against java-tron as the single source of truth,
with **differential verification on live-chain data** at every step. See [`SPEC.md`](./SPEC.md)
for the design, testing strategy, and phased roadmap.

## Status

Foundational, end-to-end-tested cores across **all six SPEC phases** (P0–P5). **269 tests green**,
every commit pushed. This is a coherent node skeleton where the layers compose — not yet a
production node (see [Honest scope](#honest-scope)).

| Phase | State | Landed |
|---|---|---|
| **P0** Scaffold | ✅ | 12-crate workspace, proto codegen, booting node |
| **P1** Chain & state | core done | 19 contract types routed; typed state stores; genesis init; block store; structural block validation; RocksDB backend |
| **P2** TVM | core done | opcodes + energy (byte-exact), U256 interpreter with **memory + calldata + stateful storage**, precompiles (incl. ecrecover), unified CALL dispatch, **smart-contract execution with energy→sun fees** |
| **P3** Networking + consensus | core done | wire message codec, block-sync logic, Kademlia distance, **fork choice + reorg**, **PBFT finality**, DPoS election + timing + witness scheduling |
| **P4** APIs | serving | HTTP handlers over real state **bound on an axum socket** (getaccount/getnowblock/getblockbynum) — the tron-openapi contract |
| **P5** SR block production | core done | block assembly + signing (**produce→validate round-trip**), mempool, reward distribution |

### Verified against real java-tron data

The `verify/` harness captures live blocks/accounts over gRPC (our own generated client) and
asserts byte-equality / correct behavior:

- **`txTrieRoot`** recomputed from raw tx bytes matches the header on every fixture (incl. a
  683-tx mainnet block) — transitively proving prost serialization is byte-identical.
- **Witness + 718 tx signatures** recover correctly (sha256→secp256k1→keccak address).
- **257 real TransferContract txs** replayed through the executor with value conservation.
- **718 real txs** dispatched across 6 contract types (87% coverage, zero panics).
- **Real accounts** re-encode byte-identically and round-trip through the state store.
- **Full block validation** passes on real blocks, rejects tampered ones.
- **End-to-end pipeline**: genesis → apply block through executor → serve via HTTP handler,
  value conserved.

## Layout

```
crates/
  types      Address (21-byte 0x41, Base58Check/hex), H256, Sun
  crypto     sha256/keccak256, secp256k1 sign/recover, address derivation
  proto      prost + tonic codegen from vendored java-tron .proto (wire + gRPC)
  storage    KvStore trait; in-memory + RocksDB (feature) backends
  state      world-state stores, genesis, block store, contract code
  chain      tx id / merkle root / block id / signature recovery
  actuators  19 system-contract executors + block-apply executor + TVM bridge
  vm         TVM: opcodes, energy, U256 interpreter (memory/calldata/storage),
             precompiles, CALL dispatch
  consensus  DPoS election/timing/scheduling, reward, fork choice, PBFT finality,
             block validation, block production, mempool
  p2p        wire message codec, block-sync logic, Kademlia discovery distance
  rpc        HTTP handlers + axum server binding
  node       config + service supervisor with graceful shutdown
verify/      differential harness: fixture capture + parity/replay/e2e tests
```

## Commands

```bash
cargo test --workspace            # full suite (offline; fixtures committed)
cargo test -p tron-storage --features rocksdb   # incl. the RocksDB backend
cargo run -p tron-node            # boot the node skeleton (Ctrl-C to stop)

# capture fresh differential fixtures from a live network:
cargo run -p tron-verify --bin capture -- 69582212                     # Nile block
cargo run -p tron-verify --bin capture -- --endpoint http://grpc.trongrid.io:50051 84861706
cargo run -p tron-verify --bin capture_accounts -- <address> [...]     # accounts
```

## Honest scope

Each phase has a **real, correct core**, and they connect — but "production-ready" (the SPEC's bar)
is a multi-month effort beyond a single build. Known remaining depth: full CALL-frame model
(value transfer / nested calls / gas stipend / return data), a live async TCP channel + discovery
actor over real peer sockets, the complete 100+ endpoint API surface, full-chain differential sync
at scale with zero state divergence, and security hardening. The differential harness and
per-subsystem test tables are the scaffold to build those on with confidence.
