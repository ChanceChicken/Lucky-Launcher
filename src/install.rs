use crate::config::{Instance, Paths};
use crate::net::{self, Job};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const MANIFEST_URL: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";
const FABRIC_META: &str = "https://meta.fabricmc.net/v2";
const ASSET_BASE: &str = "https://resources.download.minecraft.net";

pub fn os_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "osx"
    } else {
        "linux"
    }
}

pub async fn manifest(client: &Client) -> Result<Value> {
    net::get_json(client, MANIFEST_URL).await
}

pub fn latest_release(manifest: &Value) -> Result<String> {
    manifest["latest"]["release"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow!("version manifest has no latest release"))
}

fn read_json(p: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(p)?)?)
}

fn write_json(p: &Path, v: &Value) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, serde_json::to_vec(v)?)?;
    Ok(())
}

pub fn client_jar(paths: &Paths, mc: &str) -> PathBuf {
    paths.version_dir(mc).join(format!("{mc}.jar"))
}

// ---------------------------------------------------------------- version metadata

/// Vanilla version JSON, cached on disk so launching works offline.
async fn vanilla_json(client: &Client, paths: &Paths, id: &str) -> Result<Value> {
    let path = paths.version_dir(id).join(format!("{id}.json"));
    if path.exists() {
        return read_json(&path);
    }
    let m = manifest(client).await?;
    let url = m["versions"]
        .as_array()
        .and_then(|vs| vs.iter().find(|v| v["id"] == id))
        .and_then(|v| v["url"].as_str())
        .ok_or_else(|| anyhow!("unknown Minecraft version '{id}'"))?
        .to_string();
    let json = net::get_json(client, &url).await?;
    write_json(&path, &json)?;
    Ok(json)
}

/// Fabric loader versions that support this Minecraft version, stable ones first.
pub async fn fabric_loaders(client: &Client, mc: &str) -> Result<Vec<String>> {
    let v = net::try_get_json(client, &format!("{FABRIC_META}/versions/loader/{mc}")).await?;
    let Some(v) = v else {
        return Ok(vec![]);
    };
    let (mut stable, mut other) = (Vec::new(), Vec::new());
    for e in v.as_array().into_iter().flatten() {
        if let Some(ver) = e["loader"]["version"].as_str() {
            if e["loader"]["stable"].as_bool().unwrap_or(false) {
                stable.push(ver.to_string());
            } else {
                other.push(ver.to_string());
            }
        }
    }
    stable.extend(other);
    Ok(stable)
}

async fn fabric_profile(client: &Client, paths: &Paths, mc: &str, loader: &str) -> Result<Value> {
    let id = format!("fabric-loader-{loader}-{mc}");
    let path = paths.version_dir(&id).join(format!("{id}.json"));
    if path.exists() {
        return read_json(&path);
    }
    let url = format!("{FABRIC_META}/versions/loader/{mc}/{loader}/profile/json");
    let json = net::get_json(client, &url).await?;
    write_json(&path, &json)?;
    Ok(json)
}

/// Overlay Fabric's profile on the vanilla one (same idea as `inheritsFrom`).
fn merge(parent: &Value, child: &Value) -> Value {
    let mut out = parent.clone();
    if let Some(main) = child.get("mainClass") {
        out["mainClass"] = main.clone();
    }
    // child libraries first so Fabric's versions win when the same library appears twice
    let mut libs = child["libraries"].as_array().cloned().unwrap_or_default();
    libs.extend(parent["libraries"].as_array().cloned().unwrap_or_default());
    out["libraries"] = Value::Array(libs);
    if child.get("arguments").is_some() {
        for key in ["jvm", "game"] {
            let mut a = parent["arguments"][key].as_array().cloned().unwrap_or_default();
            a.extend(child["arguments"][key].as_array().cloned().unwrap_or_default());
            out["arguments"][key] = Value::Array(a);
        }
    }
    out
}

// ---------------------------------------------------------------- rules + libraries

pub fn rules_allow(rules: Option<&Value>) -> bool {
    let Some(rules) = rules.and_then(|r| r.as_array()) else {
        return true;
    };
    let mut allowed = false;
    for r in rules {
        if rule_applies(r) {
            allowed = r["action"].as_str() == Some("allow");
        }
    }
    allowed
}

fn rule_applies(r: &Value) -> bool {
    // We don't enable any launcher "features" (demo mode, custom resolution, quick play).
    if r.get("features").is_some() {
        return false;
    }
    if let Some(os) = r.get("os") {
        if let Some(n) = os["name"].as_str() {
            if n != os_name() {
                return false;
            }
        }
        if let Some(a) = os["arch"].as_str() {
            let matches = match a {
                "x86" => cfg!(target_arch = "x86"),
                "arm64" | "aarch64" => cfg!(target_arch = "aarch64"),
                "x86_64" | "amd64" => cfg!(target_arch = "x86_64"),
                _ => true,
            };
            if !matches {
                return false;
            }
        }
    }
    true
}

pub struct Resolved {
    pub classpath: Vec<PathBuf>,
    pub jobs: Vec<Job>,
    pub native_jars: Vec<PathBuf>,
}

fn maven_path(name: &str) -> Option<String> {
    let p: Vec<&str> = name.split(':').collect();
    if p.len() < 3 {
        return None;
    }
    let group = p[0].replace('.', "/");
    let (artifact, version) = (p[1], p[2]);
    let classifier = p.get(3).map(|c| format!("-{c}")).unwrap_or_default();
    Some(format!("{group}/{artifact}/{version}/{artifact}-{version}{classifier}.jar"))
}

fn dedupe_key(name: &str) -> String {
    let p: Vec<&str> = name.split(':').collect();
    if p.len() >= 4 {
        format!("{}:{}:{}", p[0], p[1], p[3])
    } else if p.len() >= 2 {
        format!("{}:{}", p[0], p[1])
    } else {
        name.to_string()
    }
}

fn job_from_artifact(root: &Path, a: &Value) -> Option<Job> {
    let url = a["url"].as_str().filter(|u| !u.is_empty())?;
    let path = a["path"].as_str()?;
    Some(Job {
        url: url.to_string(),
        dest: root.join(path),
        sha1: a["sha1"].as_str().map(String::from),
        size: a["size"].as_u64(),
    })
}

pub fn resolve_libraries(paths: &Paths, json: &Value) -> Resolved {
    let lib_root = paths.libraries();
    let mut seen: HashSet<String> = HashSet::new();
    let mut r = Resolved {
        classpath: vec![],
        jobs: vec![],
        native_jars: vec![],
    };
    for lib in json["libraries"].as_array().into_iter().flatten() {
        if !rules_allow(lib.get("rules")) {
            continue;
        }
        let name = lib["name"].as_str().unwrap_or_default();

        // Old-style natives (pre-1.19): separate classifier jars that must be extracted.
        if let Some(cls) = lib["natives"][os_name()].as_str() {
            let cls = cls.replace("${arch}", "64");
            if let Some(c) = lib["downloads"]["classifiers"].get(&cls) {
                if let Some(job) = job_from_artifact(&lib_root, c) {
                    r.native_jars.push(job.dest.clone());
                    r.jobs.push(job);
                }
            }
        }

        if !seen.insert(dedupe_key(name)) {
            continue;
        }
        if let Some(a) = lib["downloads"].get("artifact") {
            if let Some(job) = job_from_artifact(&lib_root, a) {
                r.classpath.push(job.dest.clone());
                r.jobs.push(job);
            }
        } else if let (Some(base), Some(path)) = (lib["url"].as_str(), maven_path(name)) {
            // Fabric-style library: just a maven coordinate plus a repository URL.
            let base = if base.ends_with('/') {
                base.to_string()
            } else {
                format!("{base}/")
            };
            let dest = lib_root.join(&path);
            r.jobs.push(Job {
                url: format!("{base}{path}"),
                dest: dest.clone(),
                sha1: None,
                size: None,
            });
            r.classpath.push(dest);
        }
    }
    r
}

pub fn extract_natives(jars: &[PathBuf], dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for jar in jars {
        let file = std::fs::File::open(jar)?;
        let mut zip = zip::ZipArchive::new(file)?;
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            let name = entry.name().to_string();
            if entry.is_dir() || name.starts_with("META-INF") || name.contains("..") {
                continue;
            }
            let Some(fname) = Path::new(&name).file_name() else {
                continue;
            };
            let mut out = std::fs::File::create(dest.join(fname))?;
            std::io::copy(&mut entry, &mut out)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- assets + prepare

async fn ensure_assets(client: &Client, paths: &Paths, json: &Value) -> Result<()> {
    let idx = &json["assetIndex"];
    let id = idx["id"].as_str().context("version has no assetIndex id")?;
    let dest = paths.assets().join("indexes").join(format!("{id}.json"));
    let job = Job {
        url: idx["url"].as_str().context("no assetIndex url")?.to_string(),
        dest: dest.clone(),
        sha1: idx["sha1"].as_str().map(String::from),
        size: idx["size"].as_u64(),
    };
    net::fetch(client, &job).await?;
    let index = read_json(&dest)?;
    let objects = index["objects"].as_object().context("bad asset index")?;
    let jobs: Vec<Job> = objects
        .values()
        .filter_map(|o| {
            let hash = o["hash"].as_str()?;
            let prefix = hash.get(..2)?;
            Some(Job {
                url: format!("{ASSET_BASE}/{prefix}/{hash}"),
                dest: paths.assets().join("objects").join(prefix).join(hash),
                sha1: Some(hash.to_string()),
                size: o["size"].as_u64(),
            })
        })
        .collect();
    net::fetch_all(client, jobs, 32, "asset files").await
}

/// Make sure everything this instance needs is on disk, and return the merged
/// (vanilla + Fabric) version JSON. Only downloads what is missing, and works
/// offline once everything has been fetched.
pub async fn prepare(client: &Client, paths: &Paths, inst: &Instance) -> Result<Value> {
    let vanilla = vanilla_json(client, paths, &inst.mc_version).await?;
    let merged = match &inst.fabric_loader {
        Some(loader) => {
            let fabric = fabric_profile(client, paths, &inst.mc_version, loader).await?;
            merge(&vanilla, &fabric)
        }
        None => vanilla.clone(),
    };

    let mut jobs = resolve_libraries(paths, &merged).jobs;
    let c = &vanilla["downloads"]["client"];
    jobs.push(Job {
        url: c["url"].as_str().context("version has no client download")?.to_string(),
        dest: client_jar(paths, &inst.mc_version),
        sha1: c["sha1"].as_str().map(String::from),
        size: c["size"].as_u64(),
    });
    net::fetch_all(client, jobs, 16, "libraries").await?;
    ensure_assets(client, paths, &merged).await?;
    Ok(merged)
}
