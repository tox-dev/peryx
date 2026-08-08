//! The machine every table was measured on, and the scales its numbers sit against.
//!
//! A rate only means something next to the rates the hardware reaches at all, so the harness measures
//! the box alongside the servers: a memory copy, a real write to the device, a page-cache read, and a
//! 30 MB body over loopback HTTP. A warm registry reads the page cache and writes a socket, so its
//! throughput belongs between the disk and the memory scales, and a serving row far outside them is a
//! bug in the measurement rather than a fast server. That bracket is what caught the `crane`
//! subprocess that once dominated network throughput rows.
//!
//! The loopback row is a scale, not a ceiling. Its server does the least a server can do, one write of
//! one buffer, and it runs in this process rather than its own, so a registry with its own cores can
//! pass it under concurrency. Its eight-client aggregate even falls below its single-client rate, once
//! eight 30 MB streams stop fitting in cache. Read it as what a socket costs, not as a bound.
//!
//! Each baseline is the median of `ROUNDS` samples, because one sample of a socket is a coin flip.
//! The result is written to `site/data/bench/machine.toml` and rendered by the site's `machine`
//! shortcode, so the published tables always describe the machine that produced them.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::time::Instant;

use anyhow::Context as _;
use serde::Serialize;
use sysinfo::{Disks, System};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use crate::report::repo_root;
use crate::servers::http_client;

/// Matches the large-artifact workload, so the loopback row is directly comparable.
const PAYLOAD_BYTES: usize = 30 * 1024 * 1024;
/// Comfortably past the 4 MB L2, so the copy prices memory rather than cache.
const MEMORY_BYTES: usize = 256 * 1024 * 1024;
/// Matches the "8 parallel streams" row every throughput table reports.
const CLIENTS: usize = 8;
/// Disk chunk size: large enough to amortize the syscall, small enough to stay a streaming write.
const CHUNK_BYTES: usize = 8 * 1024 * 1024;
/// Samples per baseline, reduced to a median. An odd count so the median is a measured sample.
const ROUNDS: usize = 5;

/// Use smaller settings for smoke runs; published profiles use [`Self::default`].
#[derive(Clone, Copy)]
pub struct ProfileSettings {
    pub payload_bytes: usize,
    pub memory_bytes: usize,
    pub clients: usize,
    pub chunk_bytes: usize,
    pub rounds: usize,
}

#[cfg(test)]
#[path = "../tests/unit/machine.rs"]
mod tests;

impl Default for ProfileSettings {
    fn default() -> Self {
        Self {
            payload_bytes: PAYLOAD_BYTES,
            memory_bytes: MEMORY_BYTES,
            clients: CLIENTS,
            chunk_bytes: CHUNK_BYTES,
            rounds: ROUNDS,
        }
    }
}

/// Use this entry point when a run must not overwrite the checkout's published profile.
///
/// # Errors
/// Returns an error for invalid settings or failed measurements.
pub async fn write_profile(path: &Path, scratch: &Path, settings: ProfileSettings) -> anyhow::Result<()> {
    anyhow::ensure!(settings.payload_bytes > 0, "payload size must be positive");
    anyhow::ensure!(settings.clients > 0, "client count must be positive");
    anyhow::ensure!(
        settings.memory_bytes >= settings.clients,
        "memory size must cover every client"
    );
    anyhow::ensure!(settings.chunk_bytes > 0, "chunk size must be positive");
    anyhow::ensure!(settings.rounds > 0, "round count must be positive");
    let volumes = volumes(scratch);
    let profile = Machine {
        host: host(),
        baselines: baselines(scratch, &volumes, settings).await?,
        volumes,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&profile)?)?;
    println!("updated {}", path.display());
    Ok(())
}

#[derive(Serialize)]
struct Machine {
    host: Host,
    volumes: Vec<Volume>,
    baselines: Vec<Baseline>,
}

#[derive(Serialize)]
struct Host {
    model: String,
    cpu: String,
    architecture: String,
    cores: String,
    memory: String,
    os: String,
    kernel: String,
}

fn host() -> Host {
    let system = System::new_all();
    Host {
        model: model(),
        cpu: cpu(&system),
        architecture: System::cpu_arch(),
        cores: cores(system.cpus().len()),
        memory: gibibytes(system.total_memory()),
        os: or_unknown(System::long_os_version()),
        kernel: or_unknown(System::kernel_version()),
    }
}

fn cpu(system: &System) -> String {
    or_unknown(system.cpus().first().map(|cpu| cpu.brand().trim().to_owned()))
}

fn or_unknown(value: Option<String>) -> String {
    value.unwrap_or_else(|| "unknown".to_owned())
}

/// The board, which no portable API exposes: a desktop and a laptop of the same chip thermally
/// throttle differently, so a reader comparing against their own box needs to know which this was.
#[cfg(target_os = "macos")]
fn model() -> String {
    or_unknown(sysctl("hw.model"))
}

#[cfg(not(target_os = "macos"))]
fn model() -> String {
    model_at(Path::new("/sys/devices/virtual/dmi/id/product_name"))
}

#[cfg(not(target_os = "macos"))]
fn model_at(path: &Path) -> String {
    std::fs::read_to_string(path).map_or_else(|_| "unknown".to_owned(), |text| text.trim().to_owned())
}

/// Apple Silicon splits its cores into performance and efficiency halves, and a workload that scales
/// to eight readers is using both. A bare core count would hide why the parallel rows stop scaling
/// where they do.
#[cfg(target_os = "macos")]
fn cores(logical: usize) -> String {
    describe_cores(
        logical,
        sysctl("hw.perflevel0.logicalcpu"),
        sysctl("hw.perflevel1.logicalcpu"),
    )
}

#[cfg(target_os = "macos")]
fn describe_cores(logical: usize, performance: Option<String>, efficiency: Option<String>) -> String {
    match (performance, efficiency) {
        (Some(performance), Some(efficiency)) => {
            format!("{logical} ({performance} performance + {efficiency} efficiency)")
        }
        _ => logical.to_string(),
    }
}

#[cfg(not(target_os = "macos"))]
fn cores(logical: usize) -> String {
    describe_cores(logical, System::physical_core_count())
}

#[cfg(not(target_os = "macos"))]
fn describe_cores(logical: usize, physical: Option<usize>) -> String {
    physical.map_or_else(
        || logical.to_string(),
        |physical| format!("{logical} logical / {physical} physical"),
    )
}

#[cfg(target_os = "macos")]
fn sysctl(name: &str) -> Option<String> {
    let output = std::process::Command::new("sysctl").args(["-n", name]).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

#[derive(Serialize)]
struct Volume {
    role: String,
    mount: String,
    file_system: String,
    kind: String,
    size: String,
    removable: bool,
    /// Whether the disk baselines below were measured on this volume. Only one is: the servers keep
    /// every store and cache under the scratch volume, so the other disk's speed never reaches a table.
    benchmarked: bool,
}

/// The volumes the run actually touches: where the servers keep their stores, and where the
/// repository (and so the peryx binary) lives. On this box they are different disks, and only the
/// scratch volume ever carries benchmark data.
fn volumes(scratch: &Path) -> Vec<Volume> {
    let disks = Disks::new_with_refreshed_list();
    let roles = [
        (
            scratch.to_path_buf(),
            "benchmark scratch: every server's store and cache",
            true,
        ),
        (
            repo_root(),
            "the checkout the peryx binary is built and run from",
            false,
        ),
    ];
    let mut seen: Vec<Volume> = Vec::new();
    for (path, role, benchmarked) in roles {
        let Some(disk) = mount_for(&disks, &path) else { continue };
        let mount = disk.mount_point().display().to_string();
        if let Some(existing) = seen.iter_mut().find(|volume| volume.mount == mount) {
            existing.role = format!("{}; {role}", existing.role);
            existing.benchmarked |= benchmarked;
            continue;
        }
        seen.push(Volume {
            role: role.to_owned(),
            mount,
            file_system: disk.file_system().to_string_lossy().into_owned(),
            kind: disk.kind().to_string(),
            size: capacity(disk.total_space()),
            removable: disk.is_removable(),
            benchmarked,
        });
    }
    seen
}

/// The disk `path` actually lives on.
///
/// Asking `df` rather than prefix-matching mount points: macOS firmlinks `/var` onto the data
/// volume, so `/var/folders/...` shares no prefix with `/System/Volumes/Data` and the longest-prefix
/// answer would be the read-only system volume instead of the disk the bytes land on.
fn mount_for<'a>(disks: &'a Disks, path: &Path) -> Option<&'a sysinfo::Disk> {
    reported_mount(path)
        .and_then(|mount| disks.list().iter().find(|disk| disk.mount_point() == Path::new(&mount)))
        .or_else(|| longest_prefix(disks, path))
}

fn longest_prefix<'a>(disks: &'a Disks, path: &Path) -> Option<&'a sysinfo::Disk> {
    disks
        .list()
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().as_os_str().len())
}

fn reported_mount(path: &Path) -> Option<String> {
    let output = std::process::Command::new("df").arg("-P").arg(path).output().ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    // `df -P` guarantees one record per line; the mount point is the sixth field and may hold spaces.
    let fields: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    (fields.len() >= 6).then(|| fields[5..].join(" "))
}

#[derive(Serialize)]
struct Baseline {
    name: String,
    measures: String,
    single: String,
    parallel: String,
}

async fn baselines(scratch: &Path, volumes: &[Volume], settings: ProfileSettings) -> anyhow::Result<Vec<Baseline>> {
    let disk = volumes.iter().find(|volume| volume.benchmarked).map_or_else(
        || "the scratch volume".to_owned(),
        |volume| format!("{} ({})", volume.mount, volume.kind),
    );
    let mut loopback = Vec::with_capacity(2);
    for clients in [1, settings.clients] {
        loopback_http(clients, settings.payload_bytes).await?;
        let mut rounds = Vec::with_capacity(settings.rounds);
        for _ in 0..settings.rounds {
            rounds.push(loopback_http(clients, settings.payload_bytes).await?);
        }
        loopback.push(summarize(rounds));
    }
    Ok(vec![
        Baseline {
            name: "memory copy".to_owned(),
            measures: "moving bytes between two buffers larger than L2".to_owned(),
            single: repeat(settings.rounds, || Ok(memory_copy(1, settings.memory_bytes)))?,
            parallel: repeat(settings.rounds, || {
                Ok(memory_copy(settings.clients, settings.memory_bytes))
            })?,
        },
        Baseline {
            name: "disk write".to_owned(),
            measures: format!("a sequential write to {disk}, flushed to the device"),
            single: repeat(settings.rounds, || {
                disk_write(scratch, 1, settings.memory_bytes, settings.chunk_bytes)
            })?,
            parallel: repeat(settings.rounds, || {
                disk_write(scratch, settings.clients, settings.memory_bytes, settings.chunk_bytes)
            })?,
        },
        Baseline {
            name: "file read, warm".to_owned(),
            measures: format!("reading a file on {disk} that the page cache already holds, as a warm registry does"),
            single: repeat(settings.rounds, || {
                page_cache_read(scratch, 1, settings.memory_bytes, settings.chunk_bytes)
            })?,
            parallel: repeat(settings.rounds, || {
                page_cache_read(scratch, settings.clients, settings.memory_bytes, settings.chunk_bytes)
            })?,
        },
        Baseline {
            name: "minimal HTTP server".to_owned(),
            measures: "a 30 MB body over 127.0.0.1 from a server that only writes a buffer".to_owned(),
            single: loopback[0].clone(),
            parallel: loopback[1].clone(),
        },
    ])
}

/// `ROUNDS` samples reduced the way every comparison cell in the suite is: one sample of a socket or
/// a disk is a coin flip, and an unlucky thread placement onto an efficiency core moves it by 3x.
///
/// The first sample is thrown away. It pays for pages the allocator has not handed out before, and
/// including it once measured a loopback socket at a third of its steady-state rate.
fn repeat(rounds: usize, mut measure: impl FnMut() -> anyhow::Result<f64>) -> anyhow::Result<String> {
    measure()?;
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        samples.push(measure()?);
    }
    Ok(summarize(samples))
}

/// The median, carrying the spread that says whether to trust it.
#[expect(clippy::cast_precision_loss, reason = "ROUNDS is a handful of samples")]
fn summarize(mut samples: Vec<f64>) -> String {
    samples.sort_by(f64::total_cmp);
    let median = samples[samples.len() / 2];
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples.iter().map(|sample| (sample - mean).powi(2)).sum::<f64>() / samples.len() as f64;
    let spread = variance.sqrt() / mean * 100.0;
    format!("{} ±{spread:.0}%", rate(median))
}

/// Aggregate bytes per second across `workers` threads each copying its own buffer pair.
fn memory_copy(workers: usize, bytes: usize) -> f64 {
    let each = bytes / workers;
    let mut buffers: Vec<(Vec<u8>, Vec<u8>)> = (0..workers).map(|_| (vec![7u8; each], vec![0u8; each])).collect();
    // Fault every destination page in before the clock starts. A freshly allocated `Vec` is untouched
    // zero pages, so the first write to each takes a page fault, and timing that measures the virtual
    // memory system rather than the memory bus.
    for (_, target) in &mut buffers {
        target.fill(1);
    }
    let start = Instant::now();
    std::thread::scope(|scope| {
        for (source, target) in &mut buffers {
            scope.spawn(|| target.copy_from_slice(source));
        }
    });
    let elapsed = start.elapsed().as_secs_f64();
    std::hint::black_box(&buffers);
    throughput(each * workers, elapsed)
}

/// Aggregate bytes per second writing `workers` separate files, each flushed to the device.
fn disk_write(scratch: &Path, workers: usize, bytes: usize, chunk_bytes: usize) -> anyhow::Result<f64> {
    let directory = tempfile::tempdir_in(scratch)?;
    let each = bytes / workers;
    let chunk = vec![7u8; chunk_bytes];
    let mut outcomes = Vec::with_capacity(workers);
    let start = Instant::now();
    std::thread::scope(|scope| {
        // Every writer is spawned before any is joined, so the disk sees `workers` streams at once.
        let mut handles = Vec::with_capacity(workers);
        for worker in 0..workers {
            let (directory, chunk) = (directory.path(), chunk.as_slice());
            handles.push(scope.spawn(move || write_one(&directory.join(format!("write-{worker}")), chunk, each)));
        }
        for handle in handles {
            outcomes.push(handle.join().expect("a writer thread panicked"));
        }
    });
    let elapsed = start.elapsed().as_secs_f64();
    for outcome in outcomes {
        outcome?;
    }
    Ok(throughput(each * workers, elapsed))
}

fn write_one(path: &Path, chunk: &[u8], bytes: usize) -> anyhow::Result<()> {
    let mut file = File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut written = 0;
    while written < bytes {
        let span = chunk.len().min(bytes - written);
        file.write_all(&chunk[..span])?;
        written += span;
    }
    file.sync_all().context("the write did not reach the device")?;
    Ok(())
}

/// Aggregate bytes per second with `workers` threads each reading the whole cached file.
fn page_cache_read(scratch: &Path, workers: usize, bytes: usize, chunk_bytes: usize) -> anyhow::Result<f64> {
    let directory = tempfile::tempdir_in(scratch)?;
    let path = directory.path().join("read");
    write_one(&path, &vec![7u8; chunk_bytes], bytes)?;
    read_one(&path, chunk_bytes)?;
    let mut outcomes = Vec::with_capacity(workers);
    let start = Instant::now();
    std::thread::scope(|scope| {
        // Every reader is spawned before any is joined, so the reads genuinely overlap.
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(|| read_one(&path, chunk_bytes)));
        }
        for handle in handles {
            outcomes.push(handle.join().expect("a reader thread panicked"));
        }
    });
    let elapsed = start.elapsed().as_secs_f64();
    let mut total = 0;
    for outcome in outcomes {
        total += outcome?;
    }
    Ok(throughput(total, elapsed))
}

fn read_one(path: &Path, chunk_bytes: usize) -> anyhow::Result<usize> {
    let mut file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut buffer = vec![0u8; chunk_bytes];
    let mut total = 0;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(total);
        }
        total += read;
    }
}

/// Aggregate bytes per second pulling the payload from a minimal HTTP server over the loopback.
///
/// Deliberately not peryx: this is what a server that only writes a buffer achieves, which sets the
/// scale a real registry's throughput is read against.
///
/// The server gets its own runtime on its own thread, because every registry in the suite is a
/// separate process. Sharing the harness runtime would put the server's writes and the client's reads
/// on one worker pool, so a single stream would measure how two halves of one scheduler interleave
/// rather than what the socket costs.
async fn loopback_http(clients: usize, payload_bytes: usize) -> anyhow::Result<f64> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = std::thread::spawn(move || runtime.block_on(serve_loopback(listener, stopped, payload_bytes)));

    let http = http_client()?;
    let url = format!("http://{address}/payload");
    drain(&http, &url, payload_bytes).await?;
    let start = Instant::now();
    let mut streams = Vec::with_capacity(clients);
    for _ in 0..clients {
        let (http, url) = (http.clone(), url.clone());
        streams.push(tokio::spawn(async move { drain(&http, &url, payload_bytes).await }));
    }
    for stream in streams {
        stream.await??;
    }
    let elapsed = start.elapsed().as_secs_f64();
    let _ = stop.send(());
    serving.join().expect("the loopback server thread does not panic")?;
    Ok(throughput(payload_bytes * clients, elapsed))
}

/// Serve the payload on a private runtime until told to stop, reporting the bound address back.
async fn serve_loopback(
    listener: std::net::TcpListener,
    stopped: tokio::sync::oneshot::Receiver<()>,
    payload_bytes: usize,
) -> anyhow::Result<()> {
    let listener = TcpListener::from_std(listener)?;
    let body = std::sync::Arc::new(vec![7u8; payload_bytes]);
    tokio::pin!(stopped);
    loop {
        let socket = tokio::select! {
            _ = &mut stopped => return Ok(()),
            accepted = listener.accept() => accepted?.0,
        };
        let body = std::sync::Arc::clone(&body);
        tokio::spawn(async move {
            let mut socket = socket;
            let _ = socket.set_nodelay(true);
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {payload_bytes}\r\n\r\n");
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.flush().await;
        });
    }
}

async fn drain(http: &reqwest::Client, url: &str, payload_bytes: usize) -> anyhow::Result<()> {
    let mut response = http
        .get(url)
        .send()
        .await
        .context("the loopback request did not send")?;
    let mut total = 0;
    while let Some(chunk) = response.chunk().await? {
        total += chunk.len();
    }
    anyhow::ensure!(
        total == payload_bytes,
        "loopback served {total} bytes, expected {payload_bytes}"
    );
    Ok(())
}

#[expect(clippy::cast_precision_loss, reason = "byte counts here fit f64 exactly")]
fn throughput(bytes: usize, seconds: f64) -> f64 {
    bytes as f64 / seconds
}

fn rate(bytes_per_second: f64) -> String {
    if bytes_per_second >= 1e9 {
        format!("{:.1} GB/s", bytes_per_second / 1e9)
    } else {
        format!("{:.0} MB/s", bytes_per_second / 1e6)
    }
}

/// Disks are sold and reported in decimal units, so a 2 TB drive reads as 2.0 TB rather than 1.8 TiB.
#[expect(clippy::cast_precision_loss, reason = "disk sizes fit f64 exactly")]
fn capacity(bytes: u64) -> String {
    let gigabytes = bytes as f64 / 1e9;
    if gigabytes >= 1000.0 {
        format!("{:.1} TB", gigabytes / 1e3)
    } else {
        format!("{gigabytes:.1} GB")
    }
}

/// Installed memory is sold in binary units: 16 GiB of RAM is "16 GB" on every spec sheet, and
/// printing its decimal 17.2 would read as a different machine.
#[expect(clippy::cast_precision_loss, reason = "memory sizes fit f64 exactly")]
fn gibibytes(bytes: u64) -> String {
    format!("{:.0} GB", bytes as f64 / 1024.0_f64.powi(3))
}
