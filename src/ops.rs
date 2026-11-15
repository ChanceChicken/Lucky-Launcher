//! Multi-step operations used by the GUI (the CLI has equivalent code in main.rs).

use crate::config::{Instance, Paths};
use crate::{install, mods, net};
use anyhow::{bail, Context, Result};

pub async fn create_instance(
    paths: &Paths,
    name: &str,
    mc_version: &str,
    username: &str,
    bare: bool,
) -> Result<()> {
    Instance::validate_name(name)?;
    if paths.instance_dir(name).join("instance.json").exists() {
        bail!("instance '{name}' already exists");
    }
    let client = net::client()?;
    let loaders = install::fabric_loaders(&client, mc_version).await?;
    let loader = loaders
        .into_iter()
        .next()
        .with_context(|| format!("Fabric doesn't support Minecraft {mc_version} yet"))?;
    let inst = Instance {
        name: name.to_string(),
        mc_version: mc_version.to_string(),
        fabric_loader: Some(loader),
        username: username.to_string(),
        java_path: None,
        memory_mb: None,
        extra_jvm_args: vec![],
    };
    inst.save(paths)?;
    crate::lprintln!("Creating '{name}': Minecraft {mc_version}");
    install::prepare(&client, paths, &inst).await?;
    if !bare {
        crate::lprintln!("Installing performance mods...");
        let preset = mods::PERFORMANCE.iter().map(|s| s.to_string()).collect();
        mods::add(&client, paths, &inst, preset).await?;
    }
    crate::lprintln!("'{name}' is ready.");
    Ok(())
}

pub async fn update_instance(paths: &Paths, name: &str, target: &str) -> Result<()> {
    let mut inst = Instance::load(paths, name)?;
    let client = net::client()?;
    let loader = if inst.fabric_loader.is_some() {
        let loaders = install::fabric_loaders(&client, target).await?;
        Some(
            loaders
                .into_iter()
                .next()
                .with_context(|| format!("Fabric doesn't support Minecraft {target} yet"))?,
        )
    } else {
        None
    };
    crate::lprintln!("Updating '{name}': {} -> {target}", inst.mc_version);
    inst.mc_version = target.to_string();
    inst.fabric_loader = loader;
    install::prepare(&client, paths, &inst).await?;
    inst.save(paths)?;
    if inst.fabric_loader.is_some() {
        crate::lprintln!("Updating mods...");
        mods::upgrade(&client, paths, &inst).await?;
    }
    crate::lprintln!("'{name}' is now on Minecraft {target}.");
    Ok(())
}
