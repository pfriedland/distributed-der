//! Launcher that reads assets.yaml and spawns one `edge_agent` process per asset.
//! This is a convenience tool so you don’t have to manually export env vars for each agent.
//! Comments are beginner-friendly.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::{collections::HashMap, path::PathBuf, process::Stdio};
use tokio::{process::Command, task::JoinHandle};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct AssetsFile {
    sites: Vec<SiteCfg>,
    assets: Vec<AssetCfg>,
}

#[derive(Debug, Deserialize)]
struct SiteCfg {
    id: Uuid,
    name: String,
    location: String,
}

#[derive(Debug, Deserialize)]
struct AssetCfg {
    id: Uuid,
    site_id: Uuid,
    name: String,
    capacity_mwhr: f64,
    max_mw: f64,
    min_mw: f64,
    efficiency: f64,
    ramp_rate_mw_per_min: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Where to find assets.yaml; default to workspace root.
    let assets_path = std::env::var("ASSETS_PATH").unwrap_or_else(|_| "assets.yaml".to_string());
    let headend_grpc = std::env::var("HEADEND_GRPC")
        .unwrap_or_else(|_| "127.0.0.1:50070".to_string());

    let assets = load_assets(&assets_path)?;
    let sites: HashMap<Uuid, SiteCfg> = assets.sites.into_iter().map(|s| (s.id, s)).collect();

    let mut handles: Vec<JoinHandle<Result<()>>> = Vec::new();
    for asset in assets.assets {
        let site = sites
            .get(&asset.site_id)
            .with_context(|| format!("site not found for asset {}", asset.name))?;
        let envs = build_envs(&asset, site, &headend_grpc);

        // Spawn `cargo run -p edge_agent` with the envs.
        // Note: this spawns new processes; you can change to `cargo run --bin edge_agent` if desired.
        let handle = tokio::spawn(async move {
            let mut cmd = Command::new("cargo");
            cmd.arg("run")
                .arg("-p")
                .arg("edge_agent")
                .envs(envs)
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            println!("Launching agent for {}", asset.name);
            let status = cmd.status().await.context("launching agent")?;
            if !status.success() {
                anyhow::bail!("agent for {} exited with {:?}", asset.name, status);
            }
            Ok(())
        });
        handles.push(handle);
    }

    // Wait for all agents (they run until killed).
    for h in handles {
        let _ = h.await?;
    }
    Ok(())
}

fn load_assets(path: &str) -> Result<AssetsFile> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let parsed: AssetsFile = serde_yaml::from_str(&raw).context("parsing assets.yaml")?;
    Ok(parsed)
}

fn build_envs(asset: &AssetCfg, site: &SiteCfg, headend_grpc: &str) -> HashMap<String, String> {
    let mut envs = HashMap::new();
    envs.insert("HEADEND_GRPC".into(), headend_grpc.to_string());
    envs.insert("ASSET_ID".into(), asset.id.to_string());
    envs.insert("ASSET_NAME".into(), asset.name.clone());
    envs.insert("SITE_ID".into(), site.id.to_string());
    envs.insert("SITE_NAME".into(), site.name.clone());
    envs.insert("ASSET_LOCATION".into(), site.location.clone());
    envs.insert("CAPACITY_MWHR".into(), asset.capacity_mwhr.to_string());
    envs.insert("MAX_MW".into(), asset.max_mw.to_string());
    envs.insert("MIN_MW".into(), asset.min_mw.to_string());
    envs.insert("EFFICIENCY".into(), asset.efficiency.to_string());
    envs.insert(
        "RAMP_RATE_MW_PER_MIN".into(),
        asset.ramp_rate_mw_per_min.to_string(),
    );
    envs
}
