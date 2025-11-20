# uview-miner-stats

`uview-miner-stats` is a lightweight CLI that scans a full node via RPC, matches coinbase rewards against a list of unified viewing keys, and aggregates how many blocks (and how much value) each miner produced.

## Quick start
1. Copy `config.example.toml` to `config.toml`.
2. Edit `config.toml`:
   - `start_height`: earliest block height to scan (inclusive).
   - `chain`: `mainnet` or `testnet` to match your node.
   - `rpc_url`: full node RPC endpoint exposing `getblockcount/hash/block`.
   - `ufvks`: one `{ label = "...", key = "uview1..." }` entry per miner.
   - Optional: `cache_file` (local block cache) and `output_file` (where the report is written).
3. Run from the repo root (release build recommended for faster scans):
```
cargo run --release -p uview-miner-stats -- --config config.toml
```

## What it does
- Connects to the configured Zcash/Zebra RPC endpoint (`getblockcount/hash/block`).
- Derives a transparent address per block height for each UFVK (child index == block height) and checks whether the block’s coinbase outputs paid that address.
- Caches block data locally (`cache_file`) so it doesn’t re-download blocks on subsequent runs.
- Writes a JSON report (`output_file`) containing totals per miner (blocks, ZAT, WEC, percentage share).
- Prints a console table summarizing the results.

## What it does **not** do
- It never spends or shields funds—the tool is read-only and only needs viewing keys.
- It doesn’t estimate profitability or electricity costs, only counts payouts.
- It doesn’t auto-refresh; rerun manually when you need updated stats.
- It assumes the “block height == transparent child index” derivation scheme; other schemes aren’t supported.

## Configuration
Create a TOML file (see `config.example.toml`) with:
```toml
start_height = 26000
chain = "mainnet"
rpc_url = "http://127.0.0.1:8232"

ufvks = [
    { label = "Miner #1", key = "uview1..." },
    { label = "Miner #2", key = "uview1..." },
]

cache_file = "stats-cache.json"
output_file = "miner-stats.json"
```

Run:
```
cargo run --release -p uview-miner-stats -- --config config.toml
```

## Sample output
```
Processed heights 1000-1250; matched 45 blocks across 3 miners.

Miner stats for heights 1000-1250 (total 281.25 WEC):
+----------------------+------------+------------+------------+
| Label               |     Blocks |        WEC |    % Share |
+----------------------+------------+------------+------------+
| Miner Alpha         |         28 |     175.00 |     62.22% |
| Miner Beta          |         11 |      68.75 |     24.44% |
| Miner Gamma         |          6 |      37.50 |     13.33% |
+----------------------+------------+------------+------------+
```

The JSON report (`output_file`) mirrors this data for downstream processing.

## Who needs it?
Operations teams, mining pools, or individual miners who rotate transparent receivers by block height can use this tool to audit how many blocks each miner mined, the value credited, and each miner’s percentage of the scanned range—without exposing private keys or manually parsing block data.

> **Security note:** Unified viewing keys allow observers to see shielded transaction history for the associated account. Treat your UFVKs as sensitive: share them only with trusted tooling/operators, and never publish them publicly.

Each miner entry in `miner-stats.json` also includes a `detailed_blocks` array listing the block height, block hash, and payout address for every matched reward, so teams can trace exactly which blocks contributed to the totals.
