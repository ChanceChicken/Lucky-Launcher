use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Where Lucky Launcher keeps everything. Override with the LUCKY_HOME env var.
#[derive(Clone)]
pub struct Paths {
    pub root: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let root = match std::env::var_os("LUCKY_HOME") {
            Some(p) => PathBuf::from(p),
            None => ProjectDirs::from("", "", "LuckyLauncher")
                .context("could not determine a data directory")?
                .data_dir()
                .to_path_buf(),
        };
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn libraries(&self) -> PathBuf {
        self.root.join("libraries")
    }
    pub fn assets(&self) -> PathBuf {
        self.root.join("assets")
    }
    pub fn version_dir(&self, id: &str) -> PathBuf {
        self.root.join("versions").join(id)
    }
    pub fn instances(&self) -> PathBuf {
        self.root.join("instances")
    }
    pub fn instance_dir(&self, name: &str) -> PathBuf {
        self.instances().join(name)
    }
    /// The folder Minecraft sees as its game directory (saves, mods, options.txt ...).
    pub fn game_dir(&self, name: &str) -> PathBuf {
        self.instance_dir(name).join("minecraft")
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Instance {
    pub name: String,
    pub mc_version: String,
    pub fabric_loader: Option<String>,
    pub username: String,
    #[serde(default)]
    pub java_path: Option<String>,
    /// Fixed heap size in MB. None = auto-tuned from your RAM.
    #[serde(default)]
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub extra_jvm_args: Vec<String>,
}

impl Instance {
    pub fn validate_name(name: &str) -> Result<()> {
        let ok = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            bail!("instance names may only contain letters, numbers, '-' and '_'");
        }
        Ok(())
    }

    pub fn load(paths: &Paths, name: &str) -> Result<Self> {
        Self::validate_name(name)?;
        let file = paths.instance_dir(name).join("instance.json");
        let data = std::fs::read(&file)
            .with_context(|| format!("instance '{name}' not found (try `lucky list`)"))?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let dir = paths.instance_dir(&self.name);
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(paths.game_dir(&self.name))?;
        std::fs::write(dir.join("instance.json"), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn list(paths: &Paths) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        let dir = paths.instances();
        if !dir.exists() {
            return Ok(out);
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Ok(inst) = Self::load(paths, name) {
                    out.push(inst);
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}
