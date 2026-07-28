# tron-rs — Technical Specification

A production-oriented **Tron full node in Rust**. Status: **draft v1** (grounded in analysis of
`java-tron`, `opentron`, and `rust-tron`).

## 0. Decisions (locked)

| # | Decision | Choice |
|---|---|---|
| D1 | Deliverable | Critical analysis **+** reimplementation plan **+** starter scaffold |
| D2 | Build strategy | **Greenfield**, using `opentron`/`rust-tron` as reference (study the hard parts; do not inherit their code/debt) |
| D3 | Compatibility bar | **Testnet (Nile/Shasta) consensus parity first**, mainnet hardening later |
| D4 | Node role (v1) | **Full node first** (sync + validate + serve APIs); SR / DPoS block production is a later phase |

## 1. Goals / Non-goals

### Goals
- A Rust full node that syncs a Tron **testnet** from genesis over P2P and stays in **consensus parity**
  with java-tron: it accepts the same canonical chain, computes the same per-block `txTrieRoot`, and
  reaches the same per-account world state at every height.
- Serve the read/query API surface — HTTP first (contract = the `tron-openapi` spec from task 1), then
  gRPC and eth-JSON-RPC.
- Modern, maintainable Rust: **stable toolchain**, `async`/tokio, cleanly separated crates, and a
  differential-verification harness against a java-tron reference node.

### Non-goals (v1)
- Block production as a Super Representative (DPoS witness) — deferred (D4).
- Mainnet-scale performance/hardening SLAs — after testnet parity (D3).
- Shielded/zk (zTRON) — behind a feature flag, deferred.
- Reusing opentron/rust-tron as a codebase (D2) — references only.

## 2. Success criteria
- **M-parity:** replaying a testnet block range, every block's `txTrieRoot` matches the header, and a
  periodic **full-state diff against a java-tron reference node** shows zero divergence (§7).
- **M-live:** joins the live testnet, follows the head, and serves the `tron-openapi` HTTP endpoints
  with responses schema-valid against that spec.
- **M-restart:** clean shutdown + restart resumes from persisted state with no re-sync.

## 3. Reference behavior (java-tron is ground truth)

Concrete parameters (cite `java-tron`):

- **Consensus — DPoS**
  - **27 active** block-producing SRs (`MAX_ACTIVE_WITNESS_NUM`, `common/.../config/Parameter.java`);
    127 standby.
  - **Block interval 3000 ms**; producer picked per slot by `DposSlot.getScheduledWitness(slot)`.
  - **Maintenance period 6 h** (`21_600_000 ms`) — at each maintenance `MaintenanceManager.countVote`
    rebuilds the active set from votes; skips 2 slots.
  - **Block reward** 32,000,000 sun/block + 16,000,000 sun vote reward (`DynamicPropertiesStore`),
    distributed by `MortgageService`.
  - Also **PBFT** (`consensus/.../pbft/`) provides block finality on top of DPoS.
- **Resource model:** two meters — **bandwidth/net** (free + staked; ~10 sun/byte fee fallback) and
  **energy** (for contract execution; default **100 sun/energy**). Both governance-adjustable.
- **Execution:** one **actuator** per system-contract tx type (transfer, TRC10, freeze/stake, vote,
  witness, proposal, exchange, smart-contract) + the **TVM** (`actuator/.../core/vm/`, energy = gas,
  `EnergyCost.java`).
- **State:** `chainbase` module — ~37 typed KV stores (`AccountStore`, `ContractStore`, `CodeStore`,
  `StorageRowStore`, `WitnessStore`, `VotesStore`, `ProposalStore`, `DelegationStore`,
  `ExchangeV2Store`, `DynamicPropertiesStore` = global tunables, …).
- **Block commitment:** header carries **`txTrieRoot`** (SHA256 binary merkle of tx ids) always; a
  global **`accountStateRoot`** exists only when the `AllowAccountStateRoot` proposal is active
  (historically off) — Tron has **no Ethereum-style state trie by default**.
- **Governance:** proposals via `ProposalCreate/Approve/Delete` actuators; params enumerated in
  `ProposalUtil.java`; a proposal passes when **> 2/3 of the 27 active SRs** approve at maintenance,
  then mutates `DynamicPropertiesStore`.
- **APIs & ports:** HTTP `8090` (JSON gateway; ~129 fullnode paths), gRPC `50051` (services `Wallet`,
  `WalletSolidity`, `WalletExtension`, `Database`, `Monitor`, `Network`), eth-**JSON-RPC** `8545`;
  p2p `18888`.

## 4. Analysis of existing Rust implementations

### 4.1 opentron — the real node effort (dead since Oct 2021)
17 crates (only `opentron`+`cli` are workspace members; rest are path deps), edition 2018, **nightly**
(`#![feature(asm)]`), tokio 1 + a **custom Tron wire protocol** (no libp2p, no gRPC).

**Reuse as reference (the valuable parts):**
- **State execution** — `manager` (~8k LOC): block application, 10 actuators, governance
  (witness/vote/proposal/reward), resource metering. The most complete DPoS-state logic in Rust.
- **State modeling** — `state`: 17 RocksDB column families with custom per-type key encoding
  (`state/src/keys.rs`); a clean map of Tron world state. **Note:** values use opentron's *own*
  `proto.state` schema, so its DB is **not byte-compatible** with java-tron — a re-modeling, not a mirror.
- **TVM** — `tvm`: a fork of **SputnikVM/rust-evm** (`opentron/evm` `tron` branch) that already encodes
  the Tron opcode set (`0xd0–0xdb`: CALLTOKEN, STAKE, UNSTAKE, WITHDRAWREWARD, ISWITNESS, …), Tron
  precompiles (`batchvalidatesign`, `validatemultisign`, shielded) with **energy costs hardcoded to
  java-tron**, and **quirk-parity feature flags** (`has_buggy_origin` replicating java-tron's ORIGIN
  bug) gated by `TvmUpgrade` → `AllowTvm*` proposals. **This is the single most valuable reference
  asset** — it captures years of TVM-parity reverse-engineering.
- **merkle-tree** — parity-correct with java-tron's `MerkleRoot` (empty→zero, odd carried up,
  parent=`SHA256(l‖r)`); drives a correct `txTrieRoot`. Directly reusable as a reference.
- **Networking** — `services/discovery` (Kademlia UDP, done) + `services/channel` (TCP block sync,
  works as a demo). Good protocol reference.
- **proto** — a clean hand-rewrite of Tron's protobufs (proto2), prost-generated.

**What to avoid / why it stalled:**
- **No world-state root / trie** — `AllowAccountStateRoot` is never computed; contract storage is a flat
  CF. (Acceptable given §3, but means parity must be checked by direct state diff, not a root hash.)
- **Prototype-grade production surface** — chain-**fork handling** is `panic!/warn!("TODO")`
  (`manager/src/lib.rs:189`, `channel/server.rs:183`); **block production/mempool** wired but
  experimental (`unimplemented!()` keystore); **no gRPC, no JSON-RPC, no event/broadcast API** (it
  exposes GraphQL instead).
- **Stack liabilities** — nightly `asm`, edition 2018, deps ~3–4 yrs stale (prost 0.8, clap 2,
  tokio 1.13), RocksDB via the unmaintained `rocks` crate.
- **Why it died:** last real work Oct 2021; it reached "syncs and replays state" but never crossed into
  a production node (fork handling, finality, mempool, standard APIs all incomplete).

### 4.2 rust-tron — client only (reusable primitives)
A gRPC wallet/CLI, not a node. Reusable **as reference**: `crypto` (`sha256`, `keccak256`), `keys`
(`Address`, secp256k1 `KeyPair`, sign/recover), `merkle-tree`; and its `proto` build via **`tonic`**
against java-tron's *original* protos — i.e. it shows the **real gRPC contract** opentron dropped.

## 5. Target architecture (greenfield)

Stable Rust, Cargo workspace. Subsystems as crates with narrow interfaces:

```
tron-rs/
  crates/
    types/        primitives: Address (21-byte, 0x41), H256, Sun, block/tx ids
    proto/        prost + tonic codegen from java-tron .proto (wire + gRPC in lockstep)
    crypto/       secp256k1, sha256/keccak, SM2; sign/recover  (ref: rust-tron/opentron)
    storage/      RocksDB (maintained `rocksdb` crate) + typed column families + overlay/commit-rollback
    state/        world-state stores (accounts, resources, votes, witnesses, assets, contracts,
                  proposals, exchanges, dynamic-properties)  (ref: opentron/state model)
    chain/        block/tx model, txTrieRoot, genesis            (ref: opentron/chain, merkle-tree)
    actuators/    one executor per system-contract tx type       (ref: opentron/manager)
    vm/           TVM: revm adapted to Tron energy+opcodes        (ref: opentron/tvm — see 5.2)
    consensus/    DPoS validation, witness schedule, maintenance, PBFT finality  (ref: opentron/manager governance)
    p2p/          discovery (Kad UDP) + channel/sync (TCP), fork-choice   (ref: opentron/services)
    rpc/          HTTP (tron-openapi contract) -> gRPC (tonic) -> JSON-RPC
    node/         binary: config, wiring, service supervision, graceful shutdown
  verify/         differential harness vs a java-tron reference node (see 7)
```

### 5.1 Core technical choices
- **Runtime:** tokio (stable). **Storage:** `rocksdb` crate (single engine; matches the java-tron
  JDK17 profile from task 2 — simplifies parity). **Proto:** `prost` + `tonic` (gRPC for free).
- **Crypto:** `secp256k1` + `sha2`/`tiny-keccak`; SM2 behind a feature. **Errors:** `thiserror` +
  typed error enums (not opentron's `Box<dyn Error>`/`String`).
- **Determinism first:** serialization, hashing, and any float paths must match java-tron bit-for-bit
  (cf. the `Math.pow` lesson from task 2). No nightly-only features.

### 5.2 TVM decision (highest-risk)
**Target: adapt modern `revm` to Tron semantics, using opentron's `tvm` crate as the authoritative
reference** for the opcode set (`0xd0–0xdb`), precompile energy constants, and the quirk-parity flags
(`has_buggy_origin`, the `AllowTvm*` upgrade gates). Rationale: revm is actively maintained, fast, and
the Rust EVM standard; opentron's Sputnik fork encodes the *knowledge* but sits on a dead base.

**Risk gate:** run an early **spike in P2** — implement a handful of Tron opcodes + the energy meter on
revm against a small differential corpus. If bending revm's gas model to Tron energy proves
prohibitive, **fall back** to clean-room re-authoring the Sputnik-style `tron` VM as our own crate
(proven semantics, lesser base). Decide at the P2 gate, not blind.

## 6. Compatibility strategy
- **java-tron is the single source of truth.** Every consensus rule is validated by differential
  testing against a reference node, not reimplemented from prose.
- **Proto parity:** codegen from the same `.proto` files java-tron ships.
- **Per-block signal:** verify our computed `txTrieRoot` equals the block header at every height (cheap,
  continuous parity check). Compute `accountStateRoot` only if the target testnet has that proposal on.

## 7. Verification harness (the core of testnet-parity)
Because Tron headers carry no world-state root, parity is proven by **direct differential state
comparison**, not a single root hash:

1. **Reference node:** run a java-tron **testnet** full node (we already build one — task 2) as the oracle.
2. **Block/tx replay:** feed our node the same testnet blocks; assert (a) same accept/reject decision,
   (b) `txTrieRoot` matches the header each block.
3. **State diff:** at chosen heights, query the reference node's state (account balances, resources,
   votes, contract code + storage via its HTTP/gRPC APIs) and compare to ours; on mismatch, diff the
   changed keys to localize the bug. Zero divergence over a sustained range = the D3 gate.
4. **API conformance:** validate our HTTP responses against the `tron-openapi` spec (task 1) and
   contract-test against the reference node.
5. **Corpus:** capture testnet blocks + reference-state snapshots as committed fixtures for CI.

## 8. Testing strategy

The §7 differential harness is the **top** of a test pyramid, not the whole of it. Each layer catches
a class of bugs the layer above is too coarse to localize.

### 8.1 Layers
1. **Unit (per crate) — golden vectors.**
   - `crypto`: secp256k1 sign/recover, `sha256`/`keccak256`, SM2 against known test vectors.
   - `types`: address encoding (21-byte `0x41…`, Base58Check ↔ hex), `Sun` arithmetic, id types.
   - `proto`: encode/decode roundtrip; decode of real captured messages.
   - `state`: per-type key-encoding roundtrip; column-family mapping.
2. **Component — behavior in isolation.**
   - **Actuators:** one suite per system-contract type (transfer / TRC10 / freeze-stake / vote /
     witness / proposal / exchange): apply to a seeded state, assert resulting state + fees/resources.
   - **TVM opcode-level differential:** each opcode and precompile executed against java-tron for the
     same input → assert equal result **and** equal energy cost (uses opentron's `tvm` energy constants
     as the reference table; see §5.2). Table-driven so gaps are visible.
   - **Consensus units:** witness-schedule/slot math, maintenance vote-counting, proposal >2/3 rule.
3. **Property / fuzz — robustness & invariants.**
   - Decoders (`tx`, `block`, wire messages) must **never panic** on adversarial bytes (`cargo-fuzz`).
   - Roundtrip properties (encode∘decode = id), energy-monotonicity, state commit/rollback symmetry
     (`proptest`).
4. **Differential / conformance — parity (the §7 harness).**
   - Block/tx replay + per-block `txTrieRoot` == header; periodic full-state diff vs the java-tron
     reference node. This is the **D3 gate**.
5. **Integration — the whole node.**
   - Boot → replay a fixed offline range → assert state. **Restart-resume** (M-restart). **Fork/reorg**
     handling with crafted competing chains — explicitly, because this is opentron's weakest area (P3).
   - **API conformance:** serve responses and validate them against the `tron-openapi` spec (task 1),
     contract-tested against the reference node.
6. **Soak / performance (deferred, D3).** Named now with a placeholder gate: sustained live-testnet
   follow with zero anomalies; throughput/memory baselines. Hardened after parity.

### 8.2 Fixtures & oracle
- **Oracle:** a java-tron **testnet** node (we already build one — task 2) queried via HTTP/gRPC.
- **Corpus (committed):** captured testnet blocks + reference-state snapshots at chosen heights + the
  TVM opcode/precompile vector table. Versioned so CI is deterministic and offline-runnable.
- **Determinism guards:** golden hashes for serialization/merkle so drift fails fast (cf. the task-2
  `Math.pow` determinism lesson).

### 8.3 Per-phase test gates
| Phase | Must be green before advancing |
|---|---|
| P0 Scaffold | crate unit tests compile+pass; proto roundtrip; `node` boots/shuts in an integration test |
| P1 Chain/state | actuator component suites; **differential replay + state-diff green on a non-VM block range** |
| P2 TVM | opcode/precompile differential table 100% for supported ops; replay green on VM-bearing blocks |
| P3 Networking | reorg/fork integration tests; live-testnet head-follow with 0 anomalies over a soak window |
| P4 APIs | `tron-openapi` conformance + contract tests vs reference node |
| P5 SR (post-v1) | block-assembly determinism; mempool property tests; witness-schedule integration |

### 8.4 CI
- Every PR: unit + component + property (short) + differential replay over the committed corpus.
- Nightly: fuzz run + a longer live-testnet soak.
- Coverage tracked but **not** a gate — differential parity is the real gate, not line coverage.

## 9. Phased roadmap
- **P0 — Scaffold & proto** *(this deliverable starts it, D1):* workspace, crates, proto codegen,
  config, `types`/`crypto` primitives, RocksDB `storage` wiring, `node` skeleton that boots + shuts down.
- **P1 — Chain & state (offline):** block/tx model, `state` stores, actuators for core system contracts
  (transfer/TRC10/freeze/vote/witness/proposal/exchange), `txTrieRoot`; differential replay green on a
  non-VM block range.
- **P2 — TVM:** contract execution + energy meter to gas/state parity (revm spike + decision gate, §5.2).
- **P3 — Networking:** discovery + channel sync + **fork-choice/finality (PBFT)** — the part opentron
  never finished; follow the live testnet head.
- **P4 — APIs:** HTTP (tron-openapi contract) → gRPC (tonic) → eth-JSON-RPC.
- **P5 — SR / block production (post-v1, D4):** witness scheduling, mempool, block assembly, keystore.

## 10. Risks & mitigations
- **TVM parity (highest)** — energy accounting + opcode/quirk edge cases. *Mitigation:* opentron `tvm`
  as reference + the P2 spike gate + differential corpus.
- **Fork-choice & finality** — opentron's weakest area; PBFT + DPoS fork rules are subtle. *Mitigation:*
  treat P3 as first-class, test against real testnet reorgs.
- **Determinism drift** — serialization/hashing/float causing state divergence. *Mitigation:* per-block
  `txTrieRoot` check + state diff catch it early.
- **Undocumented consensus rules** — *Mitigation:* differential testing is the spec.
- **Protocol drift** — opentron targets an older protocol version; re-baseline all protos/params against
  current java-tron.
- **Scope creep** — chasing mainnet/SR before testnet parity is solid. *Mitigation:* D3/D4 gates.

## 11. Scaffold plan (P0, immediate)
Create the Cargo workspace and crate skeletons from §5, wire proto codegen from java-tron's `.proto`,
stand up `types`/`crypto`/`storage`, and a `node` binary that loads config and cleanly starts/stops an
empty service set. No consensus logic yet — just a compiling, testable skeleton the later phases fill.
