use crate::config::{Instance, Paths};
use crate::{auth, hw, install, java};
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

/// Same UUID the game derives for offline-mode players.
fn offline_uuid(name: &str) -> String {
    let mut d = md5::compute(format!("OfflinePlayer:{name}")).0;
    d[6] = (d[6] & 0x0f) | 0x30;
    d[8] = (d[8] & 0x3f) | 0x80;
    let h: String = d.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn subst(s: &str, vars: &HashMap<&'static str, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

/// Turn a version-JSON argument list (strings and rule-gated objects) into plain strings.
fn flatten(args: &Value, vars: &HashMap<&'static str, String>) -> Vec<String> {
    let mut out = Vec::new();
    for a in args.as_array().into_iter().flatten() {
        match a {
            Value::String(s) => out.push(subst(s, vars)),
            Value::Object(_) => {
                if !install::rules_allow(a.get("rules")) {
                    continue;
                }
                match &a["value"] {
                    Value::String(s) => out.push(subst(s, vars)),
                    Value::Array(vs) => {
                        for v in vs {
                            if let Some(s) = v.as_str() {
                                out.push(subst(s, vars));
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

pub async fn run(
    client: &Client,
    paths: &Paths,
    inst: &Instance,
    dry_run: bool,
    join: Option<&str>,
) -> Result<()> {
    let json = install::prepare(client, paths, inst).await?;

    let required = json["javaVersion"]["majorVersion"].as_u64().unwrap_or(8) as u32;
    let java = java::select(inst.java_path.as_deref(), required)?;

    let res = install::resolve_libraries(paths, &json);
    let natives_dir = paths.instance_dir(&inst.name).join("natives");
    if res.native_jars.is_empty() {
        std::fs::create_dir_all(&natives_dir)?;
    } else {
        let _ = std::fs::remove_dir_all(&natives_dir);
        install::extract_natives(&res.native_jars, &natives_dir)?;
    }

    let mut cp = res.classpath.clone();
    cp.push(install::client_jar(paths, &inst.mc_version));
    let sep = if cfg!(windows) { ";" } else { ":" };
    let classpath = cp
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(sep);

    let hardware = hw::detect();
    let profile = hw::recommend(&hardware, inst.memory_mb);
    let game_dir = paths.game_dir(&inst.name);
    std::fs::create_dir_all(&game_dir)?;

    let version_name = match &inst.fabric_loader {
        Some(l) => format!("fabric-loader-{l}-{}", inst.mc_version),
        None => inst.mc_version.clone(),
    };

    let mut vars: HashMap<&'static str, String> = HashMap::new();
    let offline = (
        inst.username.clone(),
        offline_uuid(&inst.username),
        "0".to_string(),
        "legacy",
    );
    let (p_name, p_uuid, p_token, p_type) = match auth::fresh_account(client, paths).await {
        Ok(Some(a)) => {
            crate::lprintln!("Signed in as {}", a.username);
            (a.username, a.uuid, a.mc_token, "msa")
        }
        Ok(None) => {
            crate::lprintln!("Not signed in: offline mode (online servers will reject you).");
            offline
        }
        Err(e) => {
            crate::lprintln!("Sign-in problem: {e:#}");
            crate::lprintln!("Continuing in offline mode.");
            offline
        }
    };
    vars.insert("auth_player_name", p_name);
    vars.insert("auth_uuid", p_uuid);
    vars.insert("auth_access_token", p_token.clone());
    vars.insert("auth_xuid", "0".into());
    vars.insert("clientid", "0".into());
    vars.insert("user_type", p_type.into());
    vars.insert("version_name", version_name);
    vars.insert("version_type", "Lucky Launcher".into());
    vars.insert("game_directory", game_dir.to_string_lossy().into_owned());
    vars.insert("assets_root", paths.assets().to_string_lossy().into_owned());
    vars.insert("game_assets", paths.assets().to_string_lossy().into_owned());
    vars.insert(
        "assets_index_name",
        json["assetIndex"]["id"].as_str().unwrap_or("legacy").to_string(),
    );
    vars.insert("natives_directory", natives_dir.to_string_lossy().into_owned());
    vars.insert("library_directory", paths.libraries().to_string_lossy().into_owned());
    vars.insert("classpath_separator", sep.into());
    vars.insert("classpath", classpath.clone());
    vars.insert("launcher_name", "LuckyLauncher".into());
    vars.insert("launcher_version", env!("CARGO_PKG_VERSION").into());

    // our tuned flags first, then anything the user added, then what the version needs
    let mut args: Vec<String> = profile.args.clone();
    args.extend(inst.extra_jvm_args.clone());
    if json["arguments"]["jvm"].is_array() {
        args.extend(flatten(&json["arguments"]["jvm"], &vars));
    } else {
        args.push(format!("-Djava.library.path={}", natives_dir.display()));
        args.push("-cp".into());
        args.push(classpath);
    }
    args.push(json["mainClass"].as_str().context("version has no mainClass")?.to_string());
    if json["arguments"]["game"].is_array() {
        args.extend(flatten(&json["arguments"]["game"], &vars));
    } else if let Some(s) = json["minecraftArguments"].as_str() {
        args.extend(s.split_whitespace().map(|a| subst(a, &vars)));
    }
    if let Some(addr) = join.map(str::trim).filter(|a| !a.is_empty()) {
        if json["arguments"]["game"].to_string().contains("quickPlayMultiplayer") {
            // Minecraft 1.20+
            args.push("--quickPlayMultiplayer".into());
            args.push(addr.to_string());
        } else {
            // older versions take host and port separately
            let (host, port) = match addr.rsplit_once(':') {
                Some((h, p)) if p.parse::<u16>().is_ok() => (h, p),
                _ => (addr, "25565"),
            };
            args.push("--server".into());
            args.push(host.to_string());
            args.push("--port".into());
            args.push(port.to_string());
        }
        crate::lprintln!("Joining {addr} on start");
    }

    crate::lprintln!(
        "Launching {} (Minecraft {}{}) with Java {} and a {} MB heap",
        inst.name,
        inst.mc_version,
        inst.fabric_loader
            .as_ref()
            .map(|l| format!(", Fabric {l}"))
            .unwrap_or_default(),
        java::major_of(&java).unwrap_or(required),
        profile.heap_mb
    );

    if dry_run {
        crate::lprintln!("\n{}", java.display());
        for a in &args {
            let shown = if p_token != "0" && a.contains(&p_token) {
                a.replace(&p_token, "<hidden>")
            } else {
                a.clone()
            };
            crate::lprintln!("  {shown:?}");
        }
        return Ok(());
    }

    let mut cmd = Command::new(&java);
    cmd.args(&args)
        .current_dir(&game_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    java::hide_console(&mut cmd);
    let mut child = cmd.spawn().context("failed to start Java")?;
    let out = child.stdout.take();
    let err = child.stderr.take();
    let t1 = out.map(|o| std::thread::spawn(move || pump(o)));
    let t2 = err.map(|e| std::thread::spawn(move || pump(e)));
    let status = child.wait().context("waiting for Minecraft")?;
    for t in [t1, t2].into_iter().flatten() {
        let _ = t.join();
    }
    if !status.success() {
        bail!("Minecraft exited with {status}");
    }
    crate::lprintln!("Minecraft closed.");
    Ok(())
}

/// Forward a child's output line by line (lossy UTF-8, so odd bytes never stall the pipe).
fn pump<R: Read>(r: R) {
    let mut reader = BufReader::new(r);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => crate::log::emit(String::from_utf8_lossy(&buf).trim_end().to_string()),
        }
    }
}
