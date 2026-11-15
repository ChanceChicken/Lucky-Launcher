use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

fn exe() -> &'static str {
    if cfg!(windows) {
        "java.exe"
    } else {
        "java"
    }
}

/// Run `java -version` and return the major version (8, 17, 21, ...).
pub fn major_of(java: &Path) -> Option<u32> {
    let mut cmd = Command::new(java);
    cmd.arg("-version");
    hide_console(&mut cmd);
    let out = cmd.output().ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let start = text.find('"')? + 1;
    let end = start + text[start..].find('"')?;
    let ver = &text[start..end]; // "21.0.4", "1.8.0_392", "17"
    let mut parts = ver.split(|c| c == '.' || c == '_' || c == '-' || c == '+');
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// Look in `dir` for a `bin/java`, descending up to `max` levels.
fn collect(dir: &Path, depth: u32, max: u32, out: &mut Vec<PathBuf>) {
    let bin = dir.join("bin").join(exe());
    if bin.is_file() {
        out.push(bin);
        return;
    }
    if depth >= max {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect(&p, depth + 1, max, out);
            }
        }
    }
}

fn candidates(explicit: Option<&str>) -> Vec<PathBuf> {
    if let Some(p) = explicit {
        return vec![PathBuf::from(p)];
    }
    let mut c: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("JAVA_HOME") {
        c.push(Path::new(&home).join("bin").join(exe()));
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(pf) = std::env::var(var) {
                for vendor in [
                    "Eclipse Adoptium",
                    "Java",
                    "Microsoft",
                    "Zulu",
                    "BellSoft",
                    "Amazon Corretto",
                    "Semeru",
                ] {
                    roots.push(Path::new(&pf).join(vendor));
                }
            }
        }
        // The official launcher downloads its own Java runtimes here - reuse them.
        if let Ok(app) = std::env::var("APPDATA") {
            roots.push(Path::new(&app).join(".minecraft").join("runtime"));
        }
    } else {
        roots.push(PathBuf::from("/usr/lib/jvm"));
        if let Ok(home) = std::env::var("HOME") {
            roots.push(Path::new(&home).join(".sdkman/candidates/java"));
            roots.push(Path::new(&home).join(".minecraft/runtime"));
        }
    }
    for r in roots {
        collect(&r, 0, 4, &mut c);
    }
    c.push(PathBuf::from("java")); // whatever is on PATH
    c
}

/// All working Java installs we can find, with their major versions.
pub fn discover(explicit: Option<&str>) -> Vec<(PathBuf, u32)> {
    let mut found: Vec<(PathBuf, u32)> = Vec::new();
    for c in candidates(explicit) {
        if found.iter().any(|(p, _)| *p == c) {
            continue;
        }
        if let Some(m) = major_of(&c) {
            found.push((c, m));
        }
    }
    found
}

/// Pick the Java to launch with: exactly the required major if available,
/// otherwise the smallest newer one.
pub fn select(explicit: Option<&str>, required: u32) -> Result<PathBuf> {
    let found = discover(explicit);
    if let Some((p, _)) = found.iter().find(|(_, m)| *m == required) {
        return Ok(p.clone());
    }
    if let Some((p, _)) = found
        .iter()
        .filter(|(_, m)| *m > required)
        .min_by_key(|(_, m)| *m)
    {
        return Ok(p.clone());
    }
    let have = if found.is_empty() {
        "no Java found".to_string()
    } else {
        found
            .iter()
            .map(|(p, m)| format!("Java {m} at {}", p.display()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    bail!(
        "this Minecraft version needs Java {required} or newer ({have}). \
         Install a JDK {required} (e.g. Eclipse Temurin) or point to one with \
         `lucky set <instance> --java <path to java>`"
    )
}

/// Stop Windows from flashing a console window when the GUI starts Java.
pub fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}
