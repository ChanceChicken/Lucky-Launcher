use sysinfo::System;

pub struct Hardware {
    pub ram_mb: u64,
    pub logical: usize,
    pub physical: usize,
    pub cpu: String,
}

pub fn detect() -> Hardware {
    let mut sys = System::new_all();
    sys.refresh_all();
    let logical = sys.cpus().len().max(1);
    Hardware {
        ram_mb: sys.total_memory() / 1024 / 1024,
        logical,
        physical: sys.physical_core_count().unwrap_or(logical),
        cpu: sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .unwrap_or_else(|| "unknown".into()),
    }
}

pub struct Profile {
    pub heap_mb: u32,
    pub args: Vec<String>,
}

/// Pick a heap size and JVM flags for this machine.
///
/// Heap: a third of your RAM, clamped to 2-6 GB (and never above half your RAM).
/// More heap is NOT faster for Minecraft - oversized heaps just make GC pauses longer.
/// Xms == Xmx so the JVM never stalls resizing the heap mid-game.
///
/// GC: G1 tuned for low pause times (based on the well-known Aikar flags, with a
/// shorter pause target because a client cares about frame-time spikes).
pub fn recommend(hw: &Hardware, override_mb: Option<u32>) -> Profile {
    let heap_mb = override_mb.unwrap_or_else(|| {
        let third = (hw.ram_mb / 3) as u32;
        let capped = third.clamp(2048, 6144).min((hw.ram_mb / 2) as u32);
        (capped / 512 * 512).max(1024)
    });

    let mut args: Vec<String> = vec![
        format!("-Xms{heap_mb}M"),
        format!("-Xmx{heap_mb}M"),
        "-XX:+UnlockExperimentalVMOptions".into(),
        "-XX:+UseG1GC".into(),
        "-XX:MaxGCPauseMillis=50".into(),
        "-XX:+ParallelRefProcEnabled".into(),
        "-XX:+DisableExplicitGC".into(),
        "-XX:G1NewSizePercent=30".into(),
        "-XX:G1MaxNewSizePercent=40".into(),
        "-XX:G1HeapRegionSize=16M".into(),
        "-XX:G1ReservePercent=20".into(),
        "-XX:G1HeapWastePercent=5".into(),
        "-XX:G1MixedGCCountTarget=4".into(),
        "-XX:InitiatingHeapOccupancyPercent=15".into(),
        "-XX:G1MixedGCLiveThresholdPercent=90".into(),
        "-XX:G1RSetUpdatingPauseTimePercent=5".into(),
        "-XX:SurvivorRatio=32".into(),
        "-XX:MaxTenuringThreshold=1".into(),
        "-XX:+PerfDisableSharedMem".into(),
    ];
    // Pre-touching commits all heap pages up front: slower start, fewer stutters later.
    // Only worth it when there is plenty of RAM to spare.
    if hw.ram_mb >= 16 * 1024 {
        args.push("-XX:+AlwaysPreTouch".into());
    }
    Profile { heap_mb, args }
}
