#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::Parser;
use pepper_sync::keys::transparent::{self, TransparentScope};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};
use zcash_primitives::legacy::keys::NonHardenedChildIndex;
use zingolib::config::{ChainType, chain_from_str};
use zingolib::wallet::keys::unified::UnifiedKeyStore;

#[derive(Parser, Debug)]
struct Cli {
    /// Path to the miner stats configuration file (TOML)
    #[arg(long, default_value = "miner-stats-config.toml")]
    config: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = MinerStatsConfig::from_file(&cli.config)?;
    let mut cache = BlockCache::load(&cfg.cache_file)?;
    let client = NodeRpcClient::new(&cfg.rpc_url)?;

    let tip_height = client.block_count().context("fetching block count")?;
    if tip_height < cfg.start_height {
        anyhow::bail!(
            "tip height {tip_height} is below configured start height {}",
            cfg.start_height
        );
    }

    let heights: Vec<u32> = (cfg.start_height..=tip_height).collect();
    let missing: Vec<u32> = heights
        .iter()
        .copied()
        .filter(|h| !cache.blocks.contains_key(h))
        .collect();
    if !missing.is_empty() {
        println!("Fetching {} blocks from RPC...", missing.len());
        let client = Arc::new(client.clone());
        let fetched: Result<Vec<CachedBlock>> = missing
            .par_iter()
            .map(|height| client.fetch_block(*height))
            .collect();
        for block in fetched? {
            cache.blocks.insert(block.height, block);
        }
        cache.last_tip = Some(tip_height);
        cache.save(&cfg.cache_file)?;
    } else if cache.last_tip != Some(tip_height) {
        cache.last_tip = Some(tip_height);
        cache.save(&cfg.cache_file)?;
    }

    let report = compute_statistics(&cfg, &cache, tip_height)?;
    report.write(&cfg.output_file)?;
    println!(
        "Processed heights {}-{}; matched {} blocks across {} miners.",
        cfg.start_height,
        tip_height,
        report.total_mined_blocks,
        report.miners.len()
    );
    print_table(&report);
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ConfigFile {
    start_height: u32,
    chain: String,
    rpc_url: String,
    ufvks: Vec<MinerConfigEntry>,
    cache_file: PathBuf,
    output_file: PathBuf,
}

#[derive(Debug, Deserialize)]
struct MinerConfigEntry {
    key: String,
    label: String,
}

#[derive(Debug)]
struct MinerStatsConfig {
    start_height: u32,
    chain: ChainType,
    rpc_url: String,
    miners: Vec<MinerEntry>,
    cache_file: PathBuf,
    output_file: PathBuf,
}

#[derive(Debug, Clone)]
struct MinerEntry {
    key: String,
    label: String,
}

impl MinerStatsConfig {
    fn from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: ConfigFile = toml::from_str(&raw)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        if cfg.ufvks.is_empty() {
            anyhow::bail!("config must contain at least one UFVK entry");
        }
        if let Some(parent) = cfg.cache_file.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("creating cache file directory {}", parent.display())
                })?;
            }
        }
        if let Some(parent) = cfg.output_file.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).with_context(|| {
                    format!("creating output file directory {}", parent.display())
                })?;
            }
        }
        let chain = chain_from_str(&cfg.chain)
            .map_err(|e| anyhow::anyhow!("invalid chain '{}': {}", cfg.chain, e))?;
        let miners = cfg
            .ufvks
            .into_iter()
            .map(|entry| MinerEntry {
                key: entry.key,
                label: entry.label,
            })
            .collect();

        Ok(Self {
            start_height: cfg.start_height,
            chain,
            rpc_url: cfg.rpc_url,
            miners,
            cache_file: cfg.cache_file,
            output_file: cfg.output_file,
        })
    }
}

#[derive(Default, Serialize, Deserialize)]
struct BlockCache {
    last_tip: Option<u32>,
    blocks: BTreeMap<u32, CachedBlock>,
}

impl BlockCache {
    fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read(path).with_context(|| format!("reading cache {}", path.display()))?;
        let cache = serde_json::from_slice(&raw)
            .with_context(|| format!("parsing cache {}", path.display()))?;
        Ok(cache)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)
            .with_context(|| format!("serializing cache {}", path.display()))?;
        fs::write(path, json).with_context(|| format!("writing cache {}", path.display()))
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedBlock {
    height: u32,
    hash: String,
    outputs: Vec<CoinbaseOutput>,
}

#[derive(Clone, Serialize, Deserialize)]
struct CoinbaseOutput {
    value_zat: i64,
    addresses: Vec<String>,
}

#[derive(Clone)]
struct NodeRpcClient {
    client: reqwest::blocking::Client,
    url: String,
}

impl NodeRpcClient {
    fn new(url: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("constructing RPC client")?;
        Ok(Self {
            client,
            url: url.to_string(),
        })
    }

    fn block_count(&self) -> Result<u32> {
        self.call_method::<u32>("getblockcount", serde_json::json!([]))
    }

    fn fetch_block(&self, height: u32) -> Result<CachedBlock> {
        let hash: String = self.call_method("getblockhash", serde_json::json!([height]))?;
        let block: BlockResult =
            self.call_method("getblock", serde_json::json!([hash.clone(), 2]))?;
        let outputs = block.coinbase_outputs();
        Ok(CachedBlock {
            height,
            hash,
            outputs,
        })
    }

    fn call_method<T: for<'a> Deserialize<'a>>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let request = RpcRequest {
            jsonrpc: "2.0",
            id: "zingo-miner-stats",
            method,
            params,
        };
        let response = self
            .client
            .post(&self.url)
            .json(&request)
            .send()
            .with_context(|| format!("calling RPC method {method}"))?;
        let status = response.status();
        if !status.is_success() {
            anyhow::bail!("RPC {method} failed: HTTP {status}");
        }
        let rpc_response: RpcResponse<T> = response.json().context("parsing RPC response")?;
        if let Some(err) = rpc_response.error {
            anyhow::bail!("RPC error {}: {}", err.code, err.message);
        }
        rpc_response
            .result
            .ok_or_else(|| anyhow::anyhow!("RPC {method} returned no result"))
    }
}

#[derive(Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'a str,
    id: &'a str,
    method: &'a str,
    params: serde_json::Value,
}

#[derive(Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
    #[allow(dead_code)]
    id: serde_json::Value,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Deserialize)]
struct BlockResult {
    hash: String,
    height: u32,
    tx: Vec<BlockTx>,
}

impl BlockResult {
    fn coinbase_outputs(&self) -> Vec<CoinbaseOutput> {
        self.tx
            .first()
            .map(|tx| {
                tx.vout
                    .iter()
                    .map(|vout| CoinbaseOutput {
                        value_zat: vout.value_zat,
                        addresses: vout.script_pub_key.addresses.clone().unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct BlockTx {
    vout: Vec<BlockVout>,
}

#[derive(Deserialize)]
struct BlockVout {
    #[serde(rename = "valueZat")]
    value_zat: i64,
    #[serde(rename = "scriptPubKey")]
    script_pub_key: ScriptPubKey,
}

#[derive(Deserialize)]
struct ScriptPubKey {
    addresses: Option<Vec<String>>,
}

fn compute_statistics(
    cfg: &MinerStatsConfig,
    cache: &BlockCache,
    tip_height: u32,
) -> Result<MinerStatsReport> {
    let heights: Vec<u32> = (cfg.start_height..=tip_height).collect();
    let total_blocks = heights.len() as u32;
    let coinbase_totals: HashMap<u32, i64> = heights
        .iter()
        .filter_map(|h| {
            cache.blocks.get(h).map(|block| {
                let sum: i64 = block.outputs.iter().map(|o| o.value_zat).sum();
                (*h, sum)
            })
        })
        .collect();

    let mut per_miner = Vec::new();
    let mut matched_blocks = BTreeSet::new();
    let mut block_totals: HashMap<u32, i64> = HashMap::new();

    for miner in &cfg.miners {
        let key_store = UnifiedKeyStore::new_from_ufvk(&cfg.chain, miner.key.clone())
            .with_context(|| format!("decoding UFVK {}", shorten_key(&miner.key)))?;
        let mut blocks = 0u32;
        let mut total_value = 0i64;
        let mut details = Vec::new();
        for &height in &heights {
            let block = match cache.blocks.get(&height) {
                Some(block) => block,
                None => continue,
            };
            let Some(index) = NonHardenedChildIndex::from_index(height) else {
                continue;
            };
            let address = key_store
                .generate_transparent_address(index, TransparentScope::External)
                .with_context(|| format!("deriving address for height {height}"))?;
            let encoded = transparent::encode_address(&cfg.chain, address);

            let mut matched_value = 0i64;
            for output in &block.outputs {
                if output.addresses.iter().any(|addr| addr == &encoded) {
                    matched_value += output.value_zat;
                }
            }
            if matched_value > 0 {
                blocks += 1;
                total_value += matched_value;
                matched_blocks.insert(height);
                block_totals
                    .entry(height)
                    .and_modify(|v| *v = (*v).max(matched_value))
                    .or_insert(matched_value);
                details.push(MinerBlockDetail {
                    block_height: height,
                    block_hash: block.hash.clone(),
                    payout_address: encoded.clone(),
                });
            }
        }
        per_miner.push(MinerSummary {
            label: miner.label.clone(),
            matched_blocks: blocks,
            total_value_zat: total_value,
            total_value_wec: zats_to_wec(total_value),
            share_percent: 0.0,
            detailed_blocks: details,
        });
    }

    for miner in &mut per_miner {
        miner.share_percent = percent_share_blocks(miner.matched_blocks, total_blocks);
    }

    let matched_value_zat: i64 = block_totals.values().sum();
    let unmatched_blocks = total_blocks.saturating_sub(matched_blocks.len() as u32);
    let unmatched_value_zat: i64 = coinbase_totals
        .iter()
        .filter(|(height, _)| !matched_blocks.contains(height))
        .map(|(_, value)| *value)
        .sum();
    let unmatched_value_wec = zats_to_wec(unmatched_value_zat);
    let unmatched_share = percent_share_blocks(unmatched_blocks, total_blocks);

    let total_value_zat = matched_value_zat + unmatched_value_zat;
    let total_value_wec = zats_to_wec(total_value_zat);

    Ok(MinerStatsReport {
        start_height: cfg.start_height,
        end_height: tip_height,
        total_mined_blocks: matched_blocks.len() as u32,
        total_value_zat,
        total_value_wec,
        miners: per_miner
            .iter()
            .map(|m| MinerAggregate {
                label: m.label.clone(),
                matched_blocks: m.matched_blocks,
                total_value_zat: m.total_value_zat,
                total_value_wec: m.total_value_wec,
                share_percent: m.share_percent,
            })
            .collect(),
        detailed_miners: per_miner,
        unmatched: UnmatchedSummary {
            blocks: unmatched_blocks,
            total_value_zat: unmatched_value_zat,
            total_value_wec: unmatched_value_wec,
            share_percent: unmatched_share,
        },
    })
}

#[derive(Serialize)]
struct MinerAggregate {
    label: String,
    matched_blocks: u32,
    total_value_zat: i64,
    total_value_wec: f64,
    share_percent: f64,
}

#[derive(Serialize)]
struct MinerSummary {
    label: String,
    matched_blocks: u32,
    total_value_zat: i64,
    total_value_wec: f64,
    share_percent: f64,
    detailed_blocks: Vec<MinerBlockDetail>,
}

#[derive(Serialize)]
struct MinerBlockDetail {
    block_height: u32,
    block_hash: String,
    payout_address: String,
}

#[derive(Serialize)]
struct MinerStatsReport {
    start_height: u32,
    end_height: u32,
    total_mined_blocks: u32,
    total_value_zat: i64,
    total_value_wec: f64,
    miners: Vec<MinerAggregate>,
    detailed_miners: Vec<MinerSummary>,
    #[serde(skip_serializing)]
    unmatched: UnmatchedSummary,
}

#[derive(Serialize)]
struct UnmatchedSummary {
    blocks: u32,
    total_value_zat: i64,
    total_value_wec: f64,
    share_percent: f64,
}

impl MinerStatsReport {
    fn write(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)
            .with_context(|| format!("serializing report {}", path.display()))?;
        fs::write(path, json).with_context(|| format!("writing report {}", path.display()))
    }
}

fn shorten_key(key: &str) -> String {
    if key.len() <= 16 {
        key.to_string()
    } else {
        format!("{}…{}", &key[..8], &key[key.len() - 8..])
    }
}

fn zats_to_wec(zats: i64) -> f64 {
    let wec = (zats as f64) / 100_000_000.0;
    (wec * 100.0).round() / 100.0
}

fn percent_share_blocks(part: u32, total: u32) -> f64 {
    if total == 0 {
        0.0
    } else {
        (((part as f64) / (total as f64)) * 100.0 * 100.0).round() / 100.0
    }
}

fn print_table(report: &MinerStatsReport) {
    println!(
        "\nMiner stats for heights {}-{} (total {:.2} WEC):",
        report.start_height, report.end_height, report.total_value_wec
    );
    println!("+----------------------+------------+------------+------------+");
    println!(
        "| {:<20} | {:>10} | {:>10} | {:>10} |",
        "Label", "Blocks", "WEC", "% Share"
    );
    println!("+----------------------+------------+------------+------------+");
    for miner in &report.miners {
        println!(
            "| {:<20} | {:>10} | {:>10.2} | {:>9.2}% |",
            miner.label, miner.matched_blocks, miner.total_value_wec, miner.share_percent
        );
    }
    println!("| {:_<20} | {:_<10} | {:_<10} | {:_<10} |", "", "", "", "");
    println!(
        "| {:<20} | {:>10} | {:>10.2} | {:>9.2}% |",
        "Others",
        report.unmatched.blocks,
        report.unmatched.total_value_wec,
        report.unmatched.share_percent
    );
    println!("+----------------------+------------+------------+------------+");
}
