// Hide the console window in release builds on Windows.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use lucky_launcher::auth::{self, Account};
use lucky_launcher::config::{Instance, Paths};
use lucky_launcher::mods::ModEntry;
use lucky_launcher::{hw, install, launch, log, mods, net, ops};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const GREEN: egui::Color32 = egui::Color32::from_rgb(110, 200, 120);

/// State shared between the UI thread and background tasks.
#[derive(Default)]
struct Shared {
    log: Vec<String>,
    busy: Option<String>,
    dirty: bool,
    releases: Vec<String>,
    latest: Option<String>,
    /// (code, url) while a Microsoft sign-in is waiting for the user
    device: Option<(String, String)>,
}

fn push_log(s: &mut Shared, line: String) {
    s.log.push(line);
    if s.log.len() > 3000 {
        s.log.drain(0..500);
    }
}

/// Things the UI asks for; handled after drawing so we never fight the borrow checker.
enum Action {
    Play(Instance, Option<String>),
    SignIn(String),
    SignOut,
    OpenUrl(String),
    Create {
        name: String,
        version: String,
        username: String,
        bare: bool,
    },
    Update(String, String),
    UpdateMods(Instance),
    AddMod(Instance, String),
    RemoveMod(Instance, String),
    Save(Instance),
    Open(PathBuf),
}

struct LuckyApp {
    paths: Paths,
    rt: Arc<tokio::runtime::Runtime>,
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
    auto_heap_mb: u32,
    ram_mb: u64,

    instances: Vec<Instance>,
    selected: Option<String>,
    pending_select: Option<String>,
    mods: Vec<ModEntry>,

    // settings editor for the selected instance
    edit_for: String,
    edit_username: String,
    edit_auto_mem: bool,
    edit_mem: u32,
    edit_java: String,
    add_mod_text: String,

    // dialogs
    show_new: bool,
    new_name: String,
    new_version: String,
    new_username: String,
    new_bare: bool,
    confirm_update: Option<String>,

    // account / multiplayer
    account: Option<Account>,
    show_account: bool,
    client_id_text: String,
    join_text: String,
}

async fn fetch_versions() -> anyhow::Result<(Vec<String>, String)> {
    let client = net::client()?;
    let m = install::manifest(&client).await?;
    let latest = install::latest_release(&m)?;
    let releases: Vec<String> = m["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["type"] == "release")
        .filter_map(|v| v["id"].as_str().map(String::from))
        .take(40)
        .collect();
    Ok((releases, latest))
}

fn open_folder(path: &Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(path).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

fn open_url(url: &str) {
    // only ever open plain https links (the text comes from Microsoft's sign-in reply)
    let safe = url.starts_with("https://") && !url.chars().any(|c| "&|<>^\"' \n".contains(c));
    if !safe {
        return;
    }
    #[cfg(windows)]
    {
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "start", "", url]);
        lucky_launcher::java::hide_console(&mut cmd);
        let _ = cmd.spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

impl LuckyApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let paths = Paths::new().expect("could not create the data folder");
        let rt = Arc::new(tokio::runtime::Runtime::new().expect("could not start the async runtime"));
        let shared = Arc::new(Mutex::new(Shared::default()));
        let ctx = cc.egui_ctx.clone();

        // everything the launcher logs (downloads, game output...) lands in the log panel
        {
            let shared = shared.clone();
            let ctx = ctx.clone();
            log::set_sink(Box::new(move |line| {
                if let Ok(mut s) = shared.lock() {
                    push_log(&mut s, line);
                }
                ctx.request_repaint();
            }));
        }

        // fetch the list of Minecraft versions in the background
        {
            let shared = shared.clone();
            let ctx = ctx.clone();
            rt.spawn(async move {
                let res = fetch_versions().await;
                if let Ok(mut s) = shared.lock() {
                    match res {
                        Ok((releases, latest)) => {
                            s.releases = releases;
                            s.latest = Some(latest);
                        }
                        Err(e) => push_log(&mut s, format!("Could not fetch the version list: {e:#}")),
                    }
                }
                ctx.request_repaint();
            });
        }

        let hardware = hw::detect();
        let auto_heap_mb = hw::recommend(&hardware, None).heap_mb;

        let mut app = LuckyApp {
            paths,
            rt,
            shared,
            ctx,
            auto_heap_mb,
            ram_mb: hardware.ram_mb,
            instances: vec![],
            selected: None,
            pending_select: None,
            mods: vec![],
            edit_for: String::new(),
            edit_username: String::new(),
            edit_auto_mem: true,
            edit_mem: auto_heap_mb,
            edit_java: String::new(),
            add_mod_text: String::new(),
            show_new: false,
            new_name: String::new(),
            new_version: String::new(),
            new_username: "Player".into(),
            new_bare: false,
            confirm_update: None,
            account: None,
            show_account: false,
            client_id_text: String::new(),
            join_text: String::new(),
        };
        app.client_id_text = auth::client_id(&app.paths).unwrap_or_default();
        app.reload();
        app
    }

    fn current(&self) -> Option<Instance> {
        let name = self.selected.as_ref()?;
        self.instances.iter().find(|i| &i.name == name).cloned()
    }

    fn reload(&mut self) {
        self.instances = Instance::list(&self.paths).unwrap_or_default();
        if let Some(p) = self.pending_select.clone() {
            if self.instances.iter().any(|i| i.name == p) {
                self.selected = Some(p);
                self.pending_select = None;
            }
        }
        let still_there = match &self.selected {
            Some(s) => self.instances.iter().any(|i| &i.name == s),
            None => false,
        };
        if !still_there {
            self.selected = self.instances.first().map(|i| i.name.clone());
        }
        self.account = auth::load_account(&self.paths);
        self.edit_for.clear(); // forces the settings editor to reload from disk
        self.reload_mods();
    }

    fn reload_mods(&mut self) {
        self.mods = match self.current() {
            Some(inst) => mods::entries(&self.paths, &inst).unwrap_or_default(),
            None => vec![],
        };
    }

    fn sync_edit_buffers(&mut self) {
        if let Some(inst) = self.current() {
            if self.edit_for != inst.name {
                self.edit_username = inst.username.clone();
                self.edit_auto_mem = inst.memory_mb.is_none();
                self.edit_mem = inst.memory_mb.unwrap_or(self.auto_heap_mb);
                self.edit_java = inst.java_path.clone().unwrap_or_default();
                self.edit_for = inst.name.clone();
                self.reload_mods();
            }
        }
    }

    fn log_line(&self, line: String) {
        if let Ok(mut s) = self.shared.lock() {
            push_log(&mut s, line);
        }
        self.ctx.request_repaint();
    }

    /// Run a long job in the background; the UI shows `label` and disables buttons meanwhile.
    fn run_task<Fut>(&self, label: String, fut: Fut)
    where
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        if let Ok(mut s) = self.shared.lock() {
            s.busy = Some(label);
        }
        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            let res = fut.await;
            if let Ok(mut s) = shared.lock() {
                if let Err(e) = res {
                    push_log(&mut s, format!("ERROR: {e:#}"));
                }
                s.busy = None;
                s.dirty = true;
            }
            ctx.request_repaint();
        });
    }

    fn handle(&mut self, action: Action) {
        match action {
            Action::Play(inst, join) => {
                let paths = self.paths.clone();
                self.run_task(format!("Playing {}...", inst.name), async move {
                    let client = net::client()?;
                    launch::run(&client, &paths, &inst, false, join.as_deref()).await
                });
            }
            Action::SignIn(client_id) => {
                let paths = self.paths.clone();
                let shared = self.shared.clone();
                let ctx = self.ctx.clone();
                self.run_task("Waiting for Microsoft sign-in...".to_string(), async move {
                    auth::save_client_id(&paths, &client_id)?;
                    let client = net::client()?;
                    let show = {
                        let shared = shared.clone();
                        let ctx = ctx.clone();
                        move |d: &auth::DeviceCode| {
                            if let Ok(mut s) = shared.lock() {
                                s.device = Some((d.user_code.clone(), d.verification_uri.clone()));
                            }
                            ctx.request_repaint();
                        }
                    };
                    let res = auth::login_device(&client, &paths, &client_id, show).await;
                    if let Ok(mut s) = shared.lock() {
                        s.device = None;
                    }
                    let acc = res?;
                    lucky_launcher::lprintln!("Signed in as {}", acc.username);
                    Ok(())
                });
            }
            Action::SignOut => {
                match auth::logout(&self.paths) {
                    Ok(()) => self.log_line("Signed out.".to_string()),
                    Err(e) => self.log_line(format!("ERROR: {e:#}")),
                }
                self.reload();
            }
            Action::OpenUrl(url) => open_url(&url),
            Action::Create { name, version, username, bare } => {
                let paths = self.paths.clone();
                self.pending_select = Some(name.clone());
                self.run_task(format!("Creating {name}..."), async move {
                    ops::create_instance(&paths, &name, &version, &username, bare).await
                });
            }
            Action::Update(name, target) => {
                let paths = self.paths.clone();
                self.run_task(format!("Updating {name} to {target}..."), async move {
                    ops::update_instance(&paths, &name, &target).await
                });
            }
            Action::UpdateMods(inst) => {
                let paths = self.paths.clone();
                self.run_task(format!("Updating mods for {}...", inst.name), async move {
                    let client = net::client()?;
                    mods::upgrade(&client, &paths, &inst).await
                });
            }
            Action::AddMod(inst, query) => {
                let paths = self.paths.clone();
                self.run_task(format!("Adding {query}..."), async move {
                    let client = net::client()?;
                    mods::add(&client, &paths, &inst, vec![query]).await
                });
            }
            Action::RemoveMod(inst, slug) => {
                if let Err(e) = mods::remove(&self.paths, &inst, &slug) {
                    self.log_line(format!("ERROR: {e:#}"));
                }
                self.reload_mods();
            }
            Action::Save(inst) => {
                match inst.save(&self.paths) {
                    Ok(()) => self.log_line(format!("Saved settings for {}", inst.name)),
                    Err(e) => self.log_line(format!("ERROR: {e:#}")),
                }
                self.reload();
            }
            Action::Open(path) => open_folder(&path),
        }
    }

    fn instance_view(
        &mut self,
        ui: &mut egui::Ui,
        inst: &Instance,
        busy: &Option<String>,
        latest: &Option<String>,
        actions: &mut Vec<Action>,
    ) {
        let is_busy = busy.is_some();
        let has_fabric = inst.fabric_loader.is_some();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.add_space(4.0);
            ui.heading(inst.name.as_str());
            ui.label(format!(
                "Minecraft {}{}",
                inst.mc_version,
                inst.fabric_loader
                    .as_ref()
                    .map(|l| format!("  -  Fabric {l}"))
                    .unwrap_or_default()
            ));
            ui.add_space(10.0);

            // ---- play
            ui.horizontal(|ui| {
                let play = egui::Button::new(egui::RichText::new("PLAY").size(22.0).strong())
                    .min_size(egui::vec2(180.0, 48.0))
                    .fill(egui::Color32::from_rgb(40, 130, 60));
                if ui.add_enabled(!is_busy, play).clicked() {
                    let j = self.join_text.trim();
                    let join = if j.is_empty() { None } else { Some(j.to_string()) };
                    actions.push(Action::Play(inst.clone(), join));
                }
                if let Some(b) = busy {
                    ui.spinner();
                    ui.label(b.as_str());
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label("Join server on launch:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.join_text)
                        .hint_text("e.g. play.example.com (optional)")
                        .desired_width(260.0),
                );
            });
            match &self.account {
                Some(a) => ui.weak(format!("Signed in as {}", a.username)),
                None => ui.weak("Offline mode: sign in (bottom left) to join online servers"),
            };
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Open game folder").clicked() {
                    actions.push(Action::Open(self.paths.game_dir(&inst.name)));
                }
                if ui.button("Open saves folder").clicked() {
                    actions.push(Action::Open(self.paths.game_dir(&inst.name).join("saves")));
                }
            });

            // ---- updates
            ui.add_space(12.0);
            ui.separator();
            ui.label(egui::RichText::new("Updates").strong());
            ui.horizontal(|ui| match latest {
                Some(l) if *l != inst.mc_version => {
                    ui.label(format!("New release: Minecraft {l}"));
                    if ui
                        .add_enabled(!is_busy, egui::Button::new(format!("Update to {l}")))
                        .clicked()
                    {
                        self.confirm_update = Some(l.clone());
                    }
                }
                Some(_) => {
                    ui.label("You're on the latest release.");
                }
                None => {
                    ui.label("Checking for new versions...");
                }
            });
            if has_fabric
                && ui
                    .add_enabled(!is_busy, egui::Button::new("Update all mods"))
                    .clicked()
            {
                actions.push(Action::UpdateMods(inst.clone()));
            }

            // ---- settings
            ui.add_space(12.0);
            ui.separator();
            ui.label(egui::RichText::new("Settings").strong());
            egui::Grid::new("settings_grid")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Player name");
                    ui.text_edit_singleline(&mut self.edit_username);
                    ui.end_row();

                    ui.label("Memory");
                    ui.horizontal(|ui| {
                        ui.checkbox(
                            &mut self.edit_auto_mem,
                            format!("Auto ({} MB)", self.auto_heap_mb),
                        );
                        if !self.edit_auto_mem {
                            ui.add(egui::Slider::new(&mut self.edit_mem, 1024..=12288).text("MB"));
                        }
                    });
                    ui.end_row();

                    ui.label("Java path");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.edit_java)
                            .hint_text("leave empty to auto-detect")
                            .desired_width(320.0),
                    );
                    ui.end_row();
                });
            ui.add_space(4.0);
            if ui
                .add_enabled(!is_busy, egui::Button::new("Save settings"))
                .clicked()
            {
                let mut updated = inst.clone();
                let name = self.edit_username.trim();
                updated.username = if name.is_empty() {
                    "Player".to_string()
                } else {
                    name.to_string()
                };
                updated.memory_mb = if self.edit_auto_mem {
                    None
                } else {
                    Some(self.edit_mem)
                };
                let java = self.edit_java.trim();
                updated.java_path = if java.is_empty() {
                    None
                } else {
                    Some(java.to_string())
                };
                actions.push(Action::Save(updated));
            }

            // ---- mods
            ui.add_space(12.0);
            ui.separator();
            ui.label(egui::RichText::new(format!("Mods ({})", self.mods.len())).strong());
            if has_fabric {
                ui.horizontal(|ui| {
                    let edit = ui.add(
                        egui::TextEdit::singleline(&mut self.add_mod_text)
                            .hint_text("mod name or Modrinth slug, e.g. iris")
                            .desired_width(300.0),
                    );
                    let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let clicked = ui
                        .add_enabled(!is_busy, egui::Button::new("Add mod"))
                        .clicked();
                    if (clicked || enter) && !is_busy && !self.add_mod_text.trim().is_empty() {
                        actions.push(Action::AddMod(
                            inst.clone(),
                            self.add_mod_text.trim().to_string(),
                        ));
                        self.add_mod_text.clear();
                    }
                });
                ui.add_space(4.0);
                for m in self.mods.clone() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{}   {}", m.slug, m.version_number));
                        if !m.explicit {
                            ui.weak("(dependency)");
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add_enabled(!is_busy, egui::Button::new("Remove").small())
                                .clicked()
                            {
                                actions.push(Action::RemoveMod(inst.clone(), m.slug.clone()));
                            }
                        });
                    });
                }
            } else {
                ui.label("This is a vanilla instance, so it doesn't support Fabric mods.");
            }
            ui.add_space(12.0);
        });
    }
}

impl eframe::App for LuckyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (busy, dirty, releases, latest, device) = {
            let mut s = self.shared.lock().unwrap();
            let dirty = s.dirty;
            s.dirty = false;
            (s.busy.clone(), dirty, s.releases.clone(), s.latest.clone(), s.device.clone())
        };
        if dirty {
            self.reload();
        }
        self.sync_edit_buffers();

        let mut actions: Vec<Action> = Vec::new();

        // ---- left: instance list
        egui::SidePanel::left("instances_panel")
            .exact_width(200.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("Lucky Launcher")
                        .size(21.0)
                        .strong()
                        .color(GREEN),
                );
                ui.add_space(4.0);
                ui.separator();
                ui.label("Instances");
                ui.add_space(2.0);
                for inst in self.instances.clone() {
                    let is_sel = self.selected.as_deref() == Some(inst.name.as_str());
                    if ui.selectable_label(is_sel, inst.name.as_str()).clicked() {
                        self.selected = Some(inst.name.clone());
                    }
                }
                ui.add_space(8.0);
                if ui.button("+ New instance").clicked() {
                    self.show_new = true;
                    self.new_version = latest.clone().unwrap_or_default();
                }
                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(6.0);
                    ui.weak(format!(
                        "{} MB RAM, auto heap {} MB",
                        self.ram_mb, self.auto_heap_mb
                    ));
                    ui.add_space(6.0);
                    match &self.account {
                        Some(a) => {
                            if ui.button("Sign out").clicked() {
                                actions.push(Action::SignOut);
                            }
                            ui.label(
                                egui::RichText::new(format!("Signed in: {}", a.username)).color(GREEN),
                            );
                        }
                        None => {
                            if ui.button("Sign in with Microsoft").clicked() {
                                self.show_account = true;
                            }
                            ui.weak("Offline mode");
                        }
                    }
                    ui.separator();
                });
            });

        // ---- bottom: log
        egui::TopBottomPanel::bottom("log_panel")
            .resizable(true)
            .default_height(170.0)
            .min_height(70.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Log").strong());
                    if ui.small_button("Clear").clicked() {
                        if let Ok(mut s) = self.shared.lock() {
                            s.log.clear();
                        }
                    }
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Ok(s) = self.shared.lock() {
                            let start = s.log.len().saturating_sub(400);
                            for line in &s.log[start..] {
                                ui.label(egui::RichText::new(line.as_str()).monospace().size(12.0));
                            }
                        }
                    });
            });

        // ---- centre: selected instance
        egui::CentralPanel::default().show(ctx, |ui| match self.current() {
            Some(inst) => self.instance_view(ui, &inst, &busy, &latest, &mut actions),
            None => {
                ui.add_space(20.0);
                ui.heading("No instances yet");
                ui.label("Click \"+ New instance\" on the left to create one.");
            }
        });

        // ---- dialog: Microsoft code to enter
        if let Some((code, uri)) = device.clone() {
            egui::Window::new("Sign in with Microsoft")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(380.0);
                    ui.label("1. Open this page in your browser:");
                    ui.horizontal(|ui| {
                        ui.monospace(uri.as_str());
                        if ui.button("Open page").clicked() {
                            actions.push(Action::OpenUrl(uri.clone()));
                        }
                    });
                    ui.add_space(6.0);
                    ui.label("2. Enter this code and approve the sign-in:");
                    ui.label(egui::RichText::new(code.as_str()).monospace().size(30.0).strong());
                    ui.add_space(6.0);
                    ui.weak("This window closes by itself once you're signed in.");
                });
        }

        // ---- dialog: start Microsoft sign-in (client id)
        if self.show_account && device.is_none() {
            let mut open = true;
            let mut go = false;
            egui::Window::new("Sign in with Microsoft")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(420.0);
                    ui.label(
                        "Lucky needs your own Azure app (client) ID to sign in. It's a one-time setup: \
                         see the README, section \"Signing in\".",
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Client ID");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.client_id_text).desired_width(300.0),
                        );
                    });
                    ui.add_space(6.0);
                    let ok = busy.is_none() && !self.client_id_text.trim().is_empty();
                    if ui.add_enabled(ok, egui::Button::new("Sign in")).clicked() {
                        go = true;
                    }
                });
            if go {
                actions.push(Action::SignIn(self.client_id_text.trim().to_string()));
            }
            self.show_account = open && !go;
        }

        // ---- dialog: new instance
        if self.show_new {
            let mut open = true;
            let mut create = false;
            let can_create = busy.is_none();
            egui::Window::new("New instance")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::Grid::new("new_instance_grid")
                        .num_columns(2)
                        .spacing([10.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Name");
                            ui.text_edit_singleline(&mut self.new_name);
                            ui.end_row();

                            ui.label("Minecraft version");
                            if releases.is_empty() {
                                ui.text_edit_singleline(&mut self.new_version);
                            } else {
                                egui::ComboBox::new("new_version_combo", "")
                                    .selected_text(self.new_version.as_str())
                                    .show_ui(ui, |ui| {
                                        for v in &releases {
                                            ui.selectable_value(
                                                &mut self.new_version,
                                                v.clone(),
                                                v.as_str(),
                                            );
                                        }
                                    });
                            }
                            ui.end_row();

                            ui.label("Player name");
                            ui.text_edit_singleline(&mut self.new_username);
                            ui.end_row();

                            ui.label("Mods");
                            ui.checkbox(&mut self.new_bare, "Skip the performance mods");
                            ui.end_row();
                        });
                    ui.add_space(8.0);
                    let ok = can_create
                        && !self.new_name.trim().is_empty()
                        && !self.new_version.trim().is_empty();
                    if ui
                        .add_enabled(ok, egui::Button::new("Create instance"))
                        .clicked()
                    {
                        create = true;
                    }
                    if !can_create {
                        ui.weak("Wait for the current task to finish first.");
                    }
                });
            if create {
                let username = if self.new_username.trim().is_empty() {
                    "Player".to_string()
                } else {
                    self.new_username.trim().to_string()
                };
                actions.push(Action::Create {
                    name: self.new_name.trim().to_string(),
                    version: self.new_version.trim().to_string(),
                    username,
                    bare: self.new_bare,
                });
                self.new_name.clear();
            }
            self.show_new = open && !create;
        }

        // ---- dialog: confirm update
        if let Some(target) = self.confirm_update.clone() {
            let sel = self.selected.clone().unwrap_or_default();
            let saves = self.paths.game_dir(&sel).join("saves");
            egui::Window::new("Update instance?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    ui.set_max_width(380.0);
                    ui.label(format!("Move '{sel}' to Minecraft {target}?"));
                    ui.add_space(4.0);
                    ui.label(
                        "Once a world has been opened in the newer version it can't be opened in an \
                         older one again. Back up your saves first if they matter to you.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Update").clicked() {
                            actions.push(Action::Update(sel.clone(), target.clone()));
                            self.confirm_update = None;
                        }
                        if ui.button("Open saves folder").clicked() {
                            actions.push(Action::Open(saves.clone()));
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_update = None;
                        }
                    });
                });
        }

        for action in actions {
            self.handle(action);
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Lucky Launcher")
            .with_inner_size([940.0, 640.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Lucky Launcher",
        options,
        Box::new(|cc| Ok(Box::new(LuckyApp::new(cc)))),
    )
}
