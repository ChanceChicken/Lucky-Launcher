use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use lucky_launcher::config::{Instance, Paths};
use lucky_launcher::{auth, hw, install, java, launch, mods, net};

#[derive(Parser)]
#[command(name = "lucky", version, about = "Lucky Launcher - a performance-first Minecraft launcher")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show detected hardware, Java installs and the JVM flags Lucky will use
    Doctor,
    /// List Minecraft versions
    Versions {
        /// Include snapshots, pre-releases etc.
        #[arg(long)]
        snapshots: bool,
        #[arg(short = 'n', long, default_value_t = 15)]
        count: usize,
    },
    /// Create an instance (Fabric + the performance mod preset by default)
    Create {
        name: String,
        /// Minecraft version (default: latest release)
        #[arg(long = "mc")]
        mc_version: Option<String>,
        /// Player name (offline mode)
        #[arg(long, default_value = "Player")]
        username: String,
        /// Don't install the performance mod preset
        #[arg(long)]
        bare: bool,
        /// Plain vanilla, no Fabric
        #[arg(long)]
        vanilla: bool,
    },
    /// List your instances
    List,
    /// Start the game
    Launch {
        name: String,
        /// Print the full java command instead of running it
        #[arg(long)]
        dry_run: bool,
        /// Join this server as soon as the game starts, e.g. play.example.com
        #[arg(long)]
        join: Option<String>,
    },
    /// Sign in with your Microsoft account (needed for online servers)
    Login {
        /// Your Azure app's client id (only needed the first time)
        #[arg(long)]
        client_id: Option<String>,
    },
    /// Sign out
    Logout,
    /// Show who is signed in
    Account,
    /// Move an instance to a newer Minecraft version (default: latest) and update its mods
    Update {
        name: String,
        #[arg(long)]
        to: Option<String>,
        /// Only report what's available, change nothing
        #[arg(long)]
        check: bool,
    },
    /// Change instance settings
    Set {
        name: String,
        /// Fixed heap size in MB (default: auto)
        #[arg(long)]
        memory: Option<u32>,
        /// Path to a specific java executable
        #[arg(long)]
        java: Option<String>,
        #[arg(long)]
        username: Option<String>,
        /// Extra JVM argument (repeatable); replaces the current extras
        #[arg(long = "jvm-arg", allow_hyphen_values = true)]
        jvm_arg: Vec<String>,
    },
    /// Manage Fabric mods (via Modrinth)
    Mod {
        #[command(subcommand)]
        cmd: ModCmd,
    },
}

#[derive(Subcommand)]
enum ModCmd {
    /// Install mods by Modrinth slug, id or name (dependencies are added automatically)
    Add { instance: String, mods: Vec<String> },
    /// Remove a mod
    Remove { instance: String, name: String },
    /// List installed mods
    List { instance: String },
    /// Search Modrinth for Fabric mods
    Search {
        query: String,
        /// Only show mods that support this instance's Minecraft version
        #[arg(long)]
        instance: Option<String>,
    },
    /// Install the performance preset (Sodium, Lithium, FerriteCore, ModernFix, ...)
    Preset { instance: String },
    /// Update all mods to their newest build for the instance's Minecraft version
    Upgrade { instance: String },
}

fn fabric_instance(paths: &Paths, name: &str) -> Result<Instance> {
    let inst = Instance::load(paths, name)?;
    if inst.fabric_loader.is_none() {
        bail!("'{name}' is a vanilla instance; mods need Fabric");
    }
    Ok(inst)
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::new()?;

    match cli.cmd {
        Cmd::Doctor => {
            let hw = hw::detect();
            println!("CPU:   {} ({} cores / {} threads)", hw.cpu, hw.physical, hw.logical);
            println!("RAM:   {} MB", hw.ram_mb);
            let p = hw::recommend(&hw, None);
            println!("Heap:  {} MB (auto)", p.heap_mb);
            println!("Flags: {}", p.args.join(" "));
            println!("Java installs found:");
            let found = java::discover(None);
            if found.is_empty() {
                println!("  none - install a JDK 21 (e.g. Eclipse Temurin)");
            }
            for (path, major) in found {
                println!("  Java {major}: {}", path.display());
            }
            println!("Data:  {}", paths.root.display());
        }

        Cmd::Versions { snapshots, count } => {
            let client = net::client()?;
            let m = install::manifest(&client).await?;
            println!(
                "latest release: {}   latest snapshot: {}",
                m["latest"]["release"].as_str().unwrap_or("?"),
                m["latest"]["snapshot"].as_str().unwrap_or("?")
            );
            for v in m["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| snapshots || v["type"] == "release")
                .take(count)
            {
                println!("  {:<16} {}", v["id"].as_str().unwrap_or("?"), v["type"].as_str().unwrap_or(""));
            }
        }

        Cmd::Create { name, mc_version, username, bare, vanilla } => {
            Instance::validate_name(&name)?;
            if paths.instance_dir(&name).join("instance.json").exists() {
                bail!("instance '{name}' already exists");
            }
            let client = net::client()?;
            let version = match mc_version {
                Some(v) => v,
                None => install::latest_release(&install::manifest(&client).await?)?,
            };
            let loader = if vanilla {
                None
            } else {
                let loaders = install::fabric_loaders(&client, &version).await?;
                Some(
                    loaders
                        .into_iter()
                        .next()
                        .with_context(|| format!("Fabric doesn't support Minecraft {version} yet (use --vanilla or pick an older version with --mc)"))?,
                )
            };
            let inst = Instance {
                name: name.clone(),
                mc_version: version,
                fabric_loader: loader,
                username,
                java_path: None,
                memory_mb: None,
                extra_jvm_args: vec![],
            };
            inst.save(&paths)?;
            println!("Creating '{name}': Minecraft {}", inst.mc_version);
            install::prepare(&client, &paths, &inst).await?;
            if inst.fabric_loader.is_some() && !bare {
                println!("Installing performance mods...");
                mods::add(&client, &paths, &inst, mods::PERFORMANCE.iter().map(|s| s.to_string()).collect()).await?;
            }
            println!("Done. Start it with: lucky launch {name}");
        }

        Cmd::List => {
            let all = Instance::list(&paths)?;
            if all.is_empty() {
                println!("no instances yet - create one with `lucky create <name>`");
            }
            for i in all {
                println!(
                    "{:<20} Minecraft {:<10} {}",
                    i.name,
                    i.mc_version,
                    i.fabric_loader.map(|l| format!("Fabric {l}")).unwrap_or_else(|| "vanilla".into())
                );
            }
        }

        Cmd::Launch { name, dry_run, join } => {
            let inst = Instance::load(&paths, &name)?;
            let client = net::client()?;
            launch::run(&client, &paths, &inst, dry_run, join.as_deref()).await?;
        }

        Cmd::Update { name, to, check } => {
            let mut inst = Instance::load(&paths, &name)?;
            let client = net::client()?;
            let target = match to {
                Some(v) => v,
                None => install::latest_release(&install::manifest(&client).await?)?,
            };
            let loaders = if inst.fabric_loader.is_some() {
                Some(install::fabric_loaders(&client, &target).await?)
            } else {
                None
            };
            if check {
                println!("'{name}' is on {}; target is {target}", inst.mc_version);
                if let Some(l) = &loaders {
                    if l.is_empty() {
                        println!("Fabric doesn't support {target} yet");
                    } else {
                        println!("Fabric loader {} is available for {target}", l[0]);
                    }
                }
                return Ok(());
            }
            let loader = match loaders {
                Some(l) => Some(
                    l.into_iter()
                        .next()
                        .with_context(|| format!("Fabric doesn't support Minecraft {target} yet"))?,
                ),
                None => None,
            };
            if target != inst.mc_version {
                println!(
                    "Updating {} -> {target}. Back up {} first: Minecraft can't open a world in an older version once it has been upgraded.",
                    inst.mc_version,
                    paths.game_dir(&name).join("saves").display()
                );
            }
            inst.mc_version = target;
            inst.fabric_loader = loader;
            install::prepare(&client, &paths, &inst).await?;
            inst.save(&paths)?;
            if inst.fabric_loader.is_some() {
                println!("Updating mods...");
                mods::upgrade(&client, &paths, &inst).await?;
            }
            println!("'{name}' is now on Minecraft {}", inst.mc_version);
        }

        Cmd::Login { client_id } => {
            if let Some(id) = client_id {
                auth::save_client_id(&paths, &id)?;
            }
            let Some(cid) = auth::client_id(&paths) else {
                bail!("no Azure client id set.\n{}", auth::SETUP_HELP);
            };
            let client = net::client()?;
            let acc = auth::login_device(&client, &paths, &cid, |d| {
                println!("\nOpen  {}\nand enter the code  {}\n\nwaiting for you to finish in the browser...", d.verification_uri, d.user_code);
            })
            .await?;
            println!("Signed in as {}", acc.username);
        }

        Cmd::Logout => {
            auth::logout(&paths)?;
            println!("signed out");
        }

        Cmd::Account => match auth::load_account(&paths) {
            Some(a) => println!("signed in as {} ({})", a.username, a.uuid),
            None => println!("not signed in (offline mode)"),
        },

        Cmd::Set { name, memory, java, username, jvm_arg } => {
            let mut inst = Instance::load(&paths, &name)?;
            if let Some(m) = memory {
                inst.memory_mb = Some(m);
            }
            if let Some(j) = java {
                inst.java_path = Some(j);
            }
            if let Some(u) = username {
                inst.username = u;
            }
            if !jvm_arg.is_empty() {
                inst.extra_jvm_args = jvm_arg;
            }
            inst.save(&paths)?;
            println!("saved");
        }

        Cmd::Mod { cmd } => {
            let client = net::client()?;
            match cmd {
                ModCmd::Add { instance, mods: wanted } => {
                    if wanted.is_empty() {
                        bail!("name at least one mod, e.g. `lucky mod add {instance} sodium`");
                    }
                    let inst = fabric_instance(&paths, &instance)?;
                    mods::add(&client, &paths, &inst, wanted).await?;
                }
                ModCmd::Remove { instance, name } => {
                    mods::remove(&paths, &fabric_instance(&paths, &instance)?, &name)?;
                }
                ModCmd::List { instance } => {
                    mods::list(&paths, &fabric_instance(&paths, &instance)?)?;
                }
                ModCmd::Search { query, instance } => {
                    let mc = match instance {
                        Some(n) => Some(Instance::load(&paths, &n)?.mc_version),
                        None => None,
                    };
                    mods::search(&client, &query, mc.as_deref()).await?;
                }
                ModCmd::Preset { instance } => {
                    let inst = fabric_instance(&paths, &instance)?;
                    mods::add(&client, &paths, &inst, mods::PERFORMANCE.iter().map(|s| s.to_string()).collect()).await?;
                }
                ModCmd::Upgrade { instance } => {
                    mods::upgrade(&client, &paths, &fabric_instance(&paths, &instance)?).await?;
                }
            }
        }
    }
    Ok(())
}
