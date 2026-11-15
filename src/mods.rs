use crate::config::{Instance, Paths};
use crate::net::{self, Job};
use anyhow::{bail, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

const API: &str = "https://api.modrinth.com/v2";

/// Fabric mods that improve FPS / stutter / memory use. `lucky mod preset` installs these.
pub const PERFORMANCE: &[&str] = &[
    "fabric-api",      // needed by most other mods
    "sodium",          // much faster renderer
    "lithium",         // game logic / tick optimisations
    "ferrite-core",    // lower memory use
    "modernfix",       // faster startup, less memory, fewer stutters
    "immediatelyfast", // faster immediate-mode rendering (GUI, entities)
    "entityculling",   // skips rendering entities you can't see
    "dynamic-fps",     // drops FPS when the window is in the background
];

#[derive(Serialize, Deserialize, Clone)]
pub struct ModEntry {
    pub slug: String,
    pub project_id: String,
    pub version_id: String,
    pub version_number: String,
    pub filename: String,
    /// true = you asked for it, false = pulled in as a dependency
    pub explicit: bool,
}

fn mods_dir(paths: &Paths, inst: &Instance) -> PathBuf {
    paths.game_dir(&inst.name).join("mods")
}

fn lock_path(paths: &Paths, inst: &Instance) -> PathBuf {
    paths.instance_dir(&inst.name).join("lucky-mods.json")
}

fn load_lock(paths: &Paths, inst: &Instance) -> Result<Vec<ModEntry>> {
    match std::fs::read(lock_path(paths, inst)) {
        Ok(d) => Ok(serde_json::from_slice(&d)?),
        Err(_) => Ok(vec![]),
    }
}

fn save_lock(paths: &Paths, inst: &Instance, lock: &[ModEntry]) -> Result<()> {
    std::fs::write(lock_path(paths, inst), serde_json::to_vec_pretty(lock)?)?;
    Ok(())
}

pub fn entries(paths: &Paths, inst: &Instance) -> Result<Vec<ModEntry>> {
    load_lock(paths, inst)
}

fn safe_name(f: &str) -> String {
    Path::new(f)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mod.jar".into())
}

/// Newest Fabric build of a project for this Minecraft version (release > beta > alpha).
async fn pick_version(client: &Client, project: &str, mc: &str) -> Result<Option<Value>> {
    let resp = client
        .get(format!("{API}/project/{project}/version"))
        .query(&[
            ("loaders", "[\"fabric\"]".to_string()),
            ("game_versions", format!("[\"{mc}\"]")),
        ])
        .send()
        .await?
        .error_for_status()?;
    let versions: Vec<Value> = resp.json().await?;
    let pick = ["release", "beta", "alpha"]
        .iter()
        .find_map(|t| versions.iter().find(|v| v["version_type"] == *t))
        .or_else(|| versions.first())
        .cloned();
    Ok(pick)
}

fn file_job(v: &Value, dir: &Path) -> Option<(Job, String)> {
    let files = v["files"].as_array()?;
    let f = files.iter().find(|x| x["primary"] == true).or_else(|| files.first())?;
    let name = safe_name(f["filename"].as_str()?);
    let job = Job {
        url: f["url"].as_str()?.to_string(),
        dest: dir.join(&name),
        sha1: f["hashes"]["sha1"].as_str().map(String::from),
        size: f["size"].as_u64(),
    };
    Some((job, name))
}

fn required_deps(v: &Value) -> Vec<String> {
    v["dependencies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["dependency_type"] == "required")
        .filter_map(|d| d["project_id"].as_str().map(String::from))
        .collect()
}

/// Accepts a slug, a project id, or free text (falls back to the top search hit).
async fn resolve_project(client: &Client, q: &str) -> Result<Option<Value>> {
    if let Some(p) = net::try_get_json(client, &format!("{API}/project/{q}")).await? {
        return Ok(Some(p));
    }
    let facets = r#"[["project_type:mod"],["categories:fabric"]]"#;
    let resp: Value = client
        .get(format!("{API}/search"))
        .query(&[("query", q), ("facets", facets), ("limit", "1")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    match resp["hits"][0]["project_id"].as_str() {
        Some(id) => net::try_get_json(client, &format!("{API}/project/{id}")).await,
        None => Ok(None),
    }
}

pub async fn search(client: &Client, query: &str, mc: Option<&str>) -> Result<()> {
    let mut facets = vec![
        r#"["project_type:mod"]"#.to_string(),
        r#"["categories:fabric"]"#.to_string(),
    ];
    if let Some(v) = mc {
        facets.push(format!(r#"["versions:{v}"]"#));
    }
    let facets = format!("[{}]", facets.join(","));
    let resp: Value = client
        .get(format!("{API}/search"))
        .query(&[("query", query), ("facets", facets.as_str()), ("limit", "8")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let hits = resp["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        crate::lprintln!("no Fabric mods found for '{query}'");
    }
    for h in hits {
        crate::lprintln!(
            "{:<28} {} - {} ({} downloads)",
            h["slug"].as_str().unwrap_or("?"),
            h["title"].as_str().unwrap_or("?"),
            h["description"].as_str().unwrap_or(""),
            h["downloads"].as_u64().unwrap_or(0)
        );
    }
    Ok(())
}

pub async fn add(client: &Client, paths: &Paths, inst: &Instance, queries: Vec<String>) -> Result<()> {
    add_inner(client, paths, inst, queries.into_iter().map(|q| (q, true)).collect()).await
}

async fn add_inner(
    client: &Client,
    paths: &Paths,
    inst: &Instance,
    queries: Vec<(String, bool)>,
) -> Result<()> {
    let dir = mods_dir(paths, inst);
    tokio::fs::create_dir_all(&dir).await?;
    let mut lock = load_lock(paths, inst)?;
    let mut queue: VecDeque<(String, bool)> = queries.into();

    while let Some((q, explicit)) = queue.pop_front() {
        let Some(proj) = resolve_project(client, &q).await? else {
            crate::lprintln!("  x no mod found for '{q}'");
            continue;
        };
        let pid = proj["id"].as_str().unwrap_or_default().to_string();
        let slug = proj["slug"].as_str().unwrap_or(&q).to_string();
        if slug != q && pid != q {
            crate::lprintln!("  ? '{q}' matched {slug}");
        }
        if let Some(e) = lock.iter_mut().find(|e| e.project_id == pid) {
            if explicit {
                e.explicit = true;
            }
            crate::lprintln!("  = {slug} already installed");
            continue;
        }
        let Some(ver) = pick_version(client, &pid, &inst.mc_version).await? else {
            crate::lprintln!("  x {slug}: no Fabric build for Minecraft {} yet", inst.mc_version);
            continue;
        };
        let Some((job, filename)) = file_job(&ver, &dir) else {
            crate::lprintln!("  x {slug}: version has no downloadable file");
            continue;
        };
        net::fetch(client, &job).await?;
        for dep in required_deps(&ver) {
            queue.push_back((dep, false));
        }
        let version_number = ver["version_number"].as_str().unwrap_or("?").to_string();
        crate::lprintln!("  + {slug} {version_number}");
        lock.push(ModEntry {
            slug,
            project_id: pid,
            version_id: ver["id"].as_str().unwrap_or_default().to_string(),
            version_number,
            filename,
            explicit,
        });
    }
    save_lock(paths, inst, &lock)
}

pub fn remove(paths: &Paths, inst: &Instance, name: &str) -> Result<()> {
    let mut lock = load_lock(paths, inst)?;
    let Some(pos) = lock.iter().position(|e| e.slug == name || e.project_id == name) else {
        bail!("'{name}' isn't tracked by Lucky Launcher (see `lucky mod list`)");
    };
    let e = lock.remove(pos);
    let _ = std::fs::remove_file(mods_dir(paths, inst).join(&e.filename));
    save_lock(paths, inst, &lock)?;
    crate::lprintln!("removed {} ({})", e.slug, e.filename);
    crate::lprintln!("note: libraries it pulled in are left alone; remove them by name if you don't need them.");
    Ok(())
}

pub fn list(paths: &Paths, inst: &Instance) -> Result<()> {
    let lock = load_lock(paths, inst)?;
    if lock.is_empty() {
        crate::lprintln!("no mods installed through Lucky Launcher yet");
    }
    for e in lock {
        crate::lprintln!(
            "{:<28} {:<16} {}",
            e.slug,
            e.version_number,
            if e.explicit { "" } else { "(dependency)" }
        );
    }
    Ok(())
}

/// Bring every tracked mod up to date for the instance's current Minecraft version.
/// Mods with no build for this version yet are moved to `mods-disabled/` so the game still starts.
pub async fn upgrade(client: &Client, paths: &Paths, inst: &Instance) -> Result<()> {
    let dir = mods_dir(paths, inst);
    let disabled = paths.game_dir(&inst.name).join("mods-disabled");
    let lock = load_lock(paths, inst)?;
    let mut keep: Vec<ModEntry> = Vec::new();
    let mut new_deps: Vec<(String, bool)> = Vec::new();

    for mut e in lock {
        match pick_version(client, &e.project_id, &inst.mc_version).await? {
            None => {
                std::fs::create_dir_all(&disabled)?;
                let _ = std::fs::rename(dir.join(&e.filename), disabled.join(&e.filename));
                crate::lprintln!(
                    "  - {}: no Fabric build for {} yet, moved to mods-disabled/",
                    e.slug, inst.mc_version
                );
            }
            Some(v) => {
                let vid = v["id"].as_str().unwrap_or_default().to_string();
                if vid != e.version_id {
                    if let Some((job, fname)) = file_job(&v, &dir) {
                        net::fetch(client, &job).await?;
                        if fname != e.filename {
                            let _ = std::fs::remove_file(dir.join(&e.filename));
                        }
                        e.version_number = v["version_number"].as_str().unwrap_or("?").to_string();
                        e.version_id = vid;
                        e.filename = fname;
                        for dep in required_deps(&v) {
                            new_deps.push((dep, false));
                        }
                        crate::lprintln!("  ^ {} -> {}", e.slug, e.version_number);
                    }
                }
                keep.push(e);
            }
        }
    }
    save_lock(paths, inst, &keep)?;
    if !new_deps.is_empty() {
        add_inner(client, paths, inst, new_deps).await?;
    }
    crate::lprintln!("mods are up to date for Minecraft {}", inst.mc_version);
    Ok(())
}
