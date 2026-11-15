# Lucky Launcher

A small, fast, performance-first Minecraft launcher written in Rust.

## Build

1. Install Rust: https://rustup.rs
2. Install a JDK (Eclipse Temurin 21 recommended). Lucky also reuses the Java that the
   official launcher downloaded into `.minecraft/runtime`, if you have it.
3. Build:

```
cargo build --release
```

This makes two programs in `target/release/`:

- `lucky-gui.exe` - the window app (double-click it, or make a desktop shortcut)
- `lucky.exe` - the command-line version, same features

Both share the same instances and settings.

## Quick start

```
lucky doctor                      # see your hardware, Java installs and the JVM flags Lucky picked
lucky create main                 # latest Minecraft + Fabric + performance mods
lucky launch main
```

## Commands

| Command | What it does |
|---|---|
| `lucky doctor` | Detects CPU/RAM/Java and shows the tuned JVM flags |
| `lucky versions [--snapshots]` | Lists Minecraft versions |
| `lucky create <name> [--mc 1.21.4] [--username Steve] [--bare] [--vanilla]` | New instance. Fabric + performance mods by default |
| `lucky list` | Lists instances |
| `lucky launch <name> [--dry-run]` | Starts the game (`--dry-run` prints the full java command) |
| `lucky update <name> [--to <ver>] [--check]` | Moves to the latest release (or `--to`), updates Fabric and all mods |
| `lucky set <name> [--memory 4096] [--java <path>] [--username X] [--jvm-arg ...]` | Instance settings |
| `lucky mod add <inst> <mods...>` | Install from Modrinth (slug, id or name); required dependencies are added automatically |
| `lucky mod remove / list / search / preset / upgrade` | Manage mods |

Mods that don't have a Fabric build for the new Minecraft version yet are moved to
`mods-disabled/` during `update` / `mod upgrade`, so the game still starts. Re-run
`lucky mod upgrade <inst>` later and add them back with `lucky mod add`.

## Where things live

Windows: `%APPDATA%\LuckyLauncher\data`, Linux: `~/.local/share/LuckyLauncher`
(override with the `LUCKY_HOME` environment variable). Libraries and assets are shared
between instances; each instance has its own `minecraft/` folder (saves, mods, options).

## How the performance tuning works

- **JVM flags** (`src/hw.rs`): G1GC tuned for short pauses (Aikar-style flags with a
  50 ms pause target), fixed heap (`Xms == Xmx`), explicit-GC disabled.
- **Heap size**: a third of your RAM, clamped to 2-6 GB. Bigger is not faster in
  Minecraft - huge heaps just make GC pauses longer. Override with `lucky set <inst> --memory N`.
- **Mods**: the preset (`src/mods.rs`) installs Fabric API, Sodium, Lithium, FerriteCore,
  ModernFix, ImmediatelyFast, Entity Culling and Dynamic FPS.
- **Launcher itself**: parallel, hash-verified downloads; nothing is re-downloaded; launching
  works offline once an instance is installed.

## Signing in (online servers)

Without signing in, Lucky launches in offline mode (single-player and offline servers only).
To join normal online servers you sign in with your Microsoft account that owns Minecraft: Java Edition.

Microsoft only allows this through an app registration of your own, so there is a one-time setup:

1. Go to https://portal.azure.com -> Microsoft Entra ID -> App registrations -> New registration.
2. Name: anything. Supported account types: **Personal Microsoft accounts only**.
3. Open the new app -> Authentication -> add platform **Mobile and desktop applications** (tick the
   `nativeclient` redirect). Under Advanced settings set **Allow public client flows** to **Yes**. Save.
4. Copy the **Application (client) ID** from the Overview page.
5. Mojang also has to approve the app for Minecraft access: https://aka.ms/AppRegInfo.
   Until it's approved, sign-in stops at the last step with "Invalid app registration".
6. Sign in:
   - GUI: "Sign in with Microsoft" (bottom left), paste the client ID, follow the code prompt.
   - CLI: `lucky login --client-id <id>`

After that, launching uses your real account automatically and refreshes the token when needed.
If sign-in ever fails, Lucky falls back to offline mode instead of refusing to start.
Your sign-in is stored in `account.json` in the data folder (plain text, like most launchers): don't share it.

To join a server straight away: `lucky launch main --join play.example.com`, or use the
"Join server on launch" box in the GUI.

## Not done yet

- Automatic Java download (Lucky finds installed Javas, it doesn't fetch one).
- Modpack import, Forge/NeoForge/Quilt.
