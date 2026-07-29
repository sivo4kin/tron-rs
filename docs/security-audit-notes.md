# ChainSecurity Java-Tron audit — mapping to tron-rs

Source: `TRON_Protocol_Security_Audit_Report.pdf` (ChainSecurity, "Limited Review
of Java-Tron", 2024-08-26; java-tron v4.7.3/v4.7.5, focus on **TVM, Consensus, P2P**).

This is the single most relevant external document for the "production-ready"
depth of our node: it is a subsystem-by-subsystem list of the exact edge cases a
faithful reimplementation must get right. Below, each finding is mapped to a
tron-rs component with current status. `Code Corrected` findings describe the
**correct** behavior we must match; `Risk Accepted` findings describe live
mainnet behavior we should be aware of (and not "fix" divergently).

The report also independently confirms a fidelity fix we just landed (§2.2.2.2):
> "Staking operations increment the global values `TOTAL_ENERGY_WEIGHT` and
> `TOTAL_NET_WEIGHT`, so that the total available daily bandwidth and energy can
> be attributed proportionally to users stakes."
That is exactly `FreezeBalanceV2`/`UnfreezeBalanceV2`/`CancelAllUnfreezeV2` wiring
`add_prop_i64` into `TOTAL_NET/ENERGY/TRON_POWER_WEIGHT` (commits 7894797, 78eef12).

## High-severity (all Code Corrected — we must implement the corrected behavior)

| # | Finding | tron-rs component | Status / action |
|---|---------|-------------------|-----------------|
| CS-JTRON-004 | **PBFT messages create state expansion** — handlers accept PBFT msgs even when PBFT inactive; `pbftCommitMessageCache` grows unbounded → OOM | `p2p` (message handling), `consensus::pbft` | TODO: gate PBFT message ingestion on an `allow_pbft` flag; bound any commit-message cache. Not yet wired — our pbft module computes finality but has no network intake yet. |
| CS-JTRON-007 | **Resource consumption by blocks not signed by witnesses** — blocks processed / stored / broadcast before the signer is checked against the active witness set | `p2p` block intake → `consensus::validation` | TODO: drop incoming blocks whose signature fails OR whose signer is not in the active witness set **before** the expensive apply/fork path. Our `validate_block` checks structure; add signer-in-witness-set as an ingest gate. |
| CS-JTRON-005 | **Unbounded memory expansion in VOTEWITNESS opcode** — memory-expansion cost underestimated by one (size-word access not priced) | `vm::energy` | Guard for when we add the `VOTEWITNESS` (0xd8) opcode: price the initial size-word access of the witness/amount arrays (java `getVoteWitnessCost2`). We do not implement 0xd8 yet. |
| CS-JTRON-006 | **Unpermissioned censoring of fork blocks** — fake blocks on top of a fork block make `switchFork()` fail and drop the legit fork block | `consensus::fork` | Same root cause as -007: validate producer before a fork block enters the Khaos/reorg set. Our `fork::should_switch`/`compute_reorg` must only consider producer-valid blocks. |

## Medium-severity

| # | Finding | tron-rs component | Status / action |
|---|---------|-------------------|-----------------|
| CS-JTRON-002 | **Accounts created with SUICIDE not charged** (Code Corrected) — `SELFDESTRUCT` to a non-existent inheritor must charge `NEW_ACCT_CALL` extra energy (via `getSuicideCost2`, proposal #91) | `vm` (SELFDESTRUCT), `vm::energy` | Guard for when we add SELFDESTRUCT: charge new-account energy when the beneficiary doesn't exist, like `CALL`. |
| CS-JTRON-012 | **Incorrect address comparison when suiciding** (Code Corrected) — java compared 20 of 21 address bytes; fixed to 21 | `vm` (SELFDESTRUCT), `types::Address` | Non-issue for us by construction: `Address` is a full 21-byte value and we compare the whole thing. Keep it that way. |
| CS-JTRON-009 | **DoS of contract creation** (Risk Accepted) — front-run account activation makes the `create` "target must not exist" check fail | `actuators::smart_contract` (create path) | Awareness only; matches mainnet. Externally-owned create checks target-not-exist; CREATE/CREATE2 differ. |
| CS-JTRON-010 | **Extra block during maintenance period** (Risk Accepted) — `getScheduledWitness` doesn't skip maintenance slots; one extra block possible in the 6s maintenance window | `consensus` (`scheduled_witness`, `slot_of`, `next_maintenance_time`) | Awareness: our scheduling must not diverge from java's `getSlot`/`getAbSlot` rounding. Do not "correct" it — nodes accept java's behavior. |
| CS-JTRON-008 | **Block interval not enforced** (Spec Changed) — block time is a multiple of 3s for honest SRs, but a malicious SR can produce a block 1–5s off; **TVM `block.timestamp` can deviate up to 2s** | `consensus` timing, `vm` (TIMESTAMP) | Awareness: don't assume exact 3s spacing when validating timestamps or exposing `block.timestamp`. |
| CS-JTRON-003 | **Forceful disconnect via relay** (Code Corrected) — block-size (and gap) check skipped for relay peers; fix does the size check for every peer | `p2p` block intake | TODO when we add relay handling: size/gap checks are peer-independent and must always run. |
| CS-JTRON-023 | **witnessStandbyCache not invalidated after reorg** (Code Corrected) — fixed by computing the standby list on the fly at end of every block | `consensus::reward` (distribution) | Design guidance: compute the standby-witness set per block during payout rather than caching across reorgs. |

## Low / informational (mostly Java-threading; Rust async avoids many by construction)

| # | Finding | tron-rs relevance |
|---|---------|-------------------|
| CS-JTRON-024 | **Ambiguous vote ordering** (Risk Accepted) — SR ranking ties broken by `hashCode`; **ordering depends on DB iteration order** | Directly relevant: our vote counting / SR election must use a **stable, explicit** total order (vote count desc, then a fixed address tie-break) and not rely on map/DB iteration order. RocksDB iterates keys lexicographically — match that if we ever tie-break by address bytes. |
| CS-JTRON-025 | Events from failed reorg not removed (Risk Accepted) | If/when we emit events: pick surviving events by (block number, block id) + longest chain, not by naive erase-on-revert. |
| CS-JTRON-028 | Known inventory will be fetched (Risk Accepted) | `p2p::sync` — we already gate fetches on head number; keep de-duplicating against known/processed inventory. |
| CS-JTRON-022 | `intValue`/`longValue` overflow (Risk Accepted) | Our U256/i64 conversions should use checked/explicit truncation; never silently mod-reduce a resource-type code. |
| CS-JTRON-003/026/030/033/034/018/038 | P2P races & cache lifecycle (Code Corrected) | Our async model (owned state per task, `Arc<WorldState>` with `&self` writes, no shared mutable `Deque`/`HashSet` popped across threads) sidesteps the `NoSuchElementException`/NPE/double-clear classes. Revisit when the real async peer state machine lands. |

## How this steers the roadmap

Transaction-execution parity is the **actuator** layer (breadth + the feature-gate /
weight / receipt depth). This audit is the checklist for the **other three**
layers' production depth:
- **P2P intake gate** (-007/-006/-003): validate signature + witness-set membership
  and size/gap **before** any expensive processing. Highest-value hardening item.
- **PBFT gating** (-004): don't accept/broadcast PBFT messages unless activated.
- **TVM opcode edges** (-005/-002/-012): VOTEWITNESS memory pricing, SELFDESTRUCT
  new-account charge + 21-byte compare — implement correctly the first time.
- **Consensus determinism** (-024/-010/-008/-023): explicit stable ordering,
  maintenance-slot handling, on-the-fly standby set.
