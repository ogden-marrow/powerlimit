//! Power limiter + auto-tuner for the Dell XPS 15 9510 (i7-11800H + RTX 3050 Ti).
//! std-only; limits are written via Intel RAPL (sysfs) and `nvidia-smi -pl`.

mod gui;

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

const RAPL: &str = "/sys/class/powercap/intel-rapl:0";
const PROFILES: &str = "/etc/powerlimit/profiles.conf";
/// GPU throttle bits that mean "too hot": SW thermal, HW slowdown, HW thermal, power brake.
const THERMAL_MASK: u64 = 0x20 | 0x08 | 0x40 | 0x80;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Limits {
    pl1: f64, // CPU sustained W
    pl2: f64, // CPU burst W
    gpu: f64, // GPU W
}

const DEFAULT: Limits = Limits { pl1: 45.0, pl2: 109.0, gpu: 40.0 };

static STOP: AtomicBool = AtomicBool::new(false);

/// println! that flushes, so a parent process reading our pipe sees lines immediately.
macro_rules! say {
    ($($a:tt)*) => {{
        println!($($a)*);
        let _ = std::io::stdout().flush();
    }};
}

extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
    fn geteuid() -> u32;
}
extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

fn is_root() -> bool {
    unsafe { geteuid() == 0 }
}

fn write_w(path: &str, watts: f64) -> Result<(), String> {
    fs::write(path, ((watts * 1e6) as u64).to_string()).map_err(|e| format!("{path}: {e}"))
}

fn read_f(path: &str) -> Option<f64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn apply(l: Limits) -> Result<(), String> {
    write_w(&format!("{RAPL}/constraint_0_power_limit_uw"), l.pl1)?;
    write_w(&format!("{RAPL}/constraint_1_power_limit_uw"), l.pl2)?;
    let out = Command::new("nvidia-smi").args(["-pl", &l.gpu.to_string()]).output();
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => eprintln!("note: GPU limit not applied: {}", String::from_utf8_lossy(&o.stdout).trim()),
        Err(e) => eprintln!("note: nvidia-smi failed: {e}"),
    }
    Ok(())
}

fn current() -> Limits {
    Limits {
        pl1: read_f(&format!("{RAPL}/constraint_0_power_limit_uw")).unwrap_or(0.0) / 1e6,
        pl2: read_f(&format!("{RAPL}/constraint_1_power_limit_uw")).unwrap_or(0.0) / 1e6,
        gpu: nvsmi("power.limit").and_then(|s| s.parse().ok()).unwrap_or(0.0),
    }
}

fn nvsmi(field: &str) -> Option<String> {
    let o = Command::new("nvidia-smi")
        .args([&format!("--query-gpu={field}"), "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn cpu_temp() -> f64 {
    for i in 0..32 {
        let dir = format!("/sys/class/hwmon/hwmon{i}");
        if fs::read_to_string(format!("{dir}/name")).map(|n| n.trim() == "coretemp").unwrap_or(false) {
            return read_f(&format!("{dir}/temp1_input")).unwrap_or(0.0) / 1000.0;
        }
    }
    0.0
}

/// Mean clock of the 8 fastest cores: a game loads a few cores, idle ones would drag a plain average down.
fn cpu_mhz() -> f64 {
    let mut f: Vec<f64> = (0..64)
        .filter_map(|i| read_f(&format!("/sys/devices/system/cpu/cpu{i}/cpufreq/scaling_cur_freq")))
        .map(|k| k / 1000.0)
        .collect();
    f.sort_by(|a, b| b.partial_cmp(a).unwrap());
    f.truncate(8);
    f.iter().sum::<f64>() / f.len().max(1) as f64
}

/// Total CPU thermal-throttle events since boot (core + package).
fn cpu_throttle_count() -> f64 {
    (0..64)
        .map(|i| {
            let d = format!("/sys/devices/system/cpu/cpu{i}/thermal_throttle");
            read_f(&format!("{d}/core_throttle_count")).unwrap_or(0.0) + read_f(&format!("{d}/package_throttle_count")).unwrap_or(0.0)
        })
        .sum()
}

struct Sample {
    cpu_t: f64,
    gpu_t: f64,
    gpu_w: f64,
    gpu_util: f64,
    gpu_hot: bool,
    gpu_mhz: f64,
}

fn sample() -> Sample {
    let line = nvsmi("temperature.gpu,power.draw,utilization.gpu,clocks_throttle_reasons.active,clocks.sm").unwrap_or_default();
    let f: Vec<&str> = line.split(',').map(str::trim).collect();
    let num = |i: usize| f.get(i).and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let reasons = f.get(3).and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()).unwrap_or(0);
    Sample { cpu_t: cpu_temp(), gpu_t: num(0), gpu_w: num(1), gpu_util: num(2), gpu_hot: reasons & THERMAL_MASK != 0, gpu_mhz: num(4) }
}

fn energy_uj() -> Option<f64> {
    read_f(&format!("{RAPL}/energy_uj"))
}

/// pids and command lines of all processes.
fn processes() -> Vec<(u32, String)> {
    let mut v = vec![];
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Ok(pid) = e.file_name().to_string_lossy().parse::<u32>() {
                if let Ok(c) = fs::read(format!("/proc/{pid}/cmdline")) {
                    v.push((pid, String::from_utf8_lossy(&c).replace('\0', " ").trim().to_string()));
                }
            }
        }
    }
    v
}

/// Short game name from a command line: exe file name, or the steamapps folder name.
fn short_name(cmd: &str) -> String {
    if let Some(i) = cmd.find("steamapps/common/") {
        let rest = &cmd[i + 17..];
        return rest.split(|c| c == '/' || c == '\\').next().unwrap_or(rest).to_string();
    }
    let first = cmd.split(' ').next().unwrap_or(cmd);
    first.rsplit(['/', '\\']).next().unwrap_or(first).to_string()
}

fn skip_proc(c: &str) -> bool {
    ["Xorg", "Xwayland", "gnome-shell", "firefox", "chrome", "steamwebhelper", "powerlimit", "steam.sh", "SteamLinuxRuntime", "pressure-vessel", "reaper", "/proton ", "/ubuntu12_32/steam"].iter().any(|x| c.contains(x))
}

/// Likely games: processes the NVIDIA GPU lists (compute or graphics), plus anything running from a Steam library.
fn candidates() -> Vec<(u32, String)> {
    let procs = processes();
    let mut v: Vec<(u32, String)> = vec![];
    if let Ok(o) = Command::new("nvidia-smi").args(["pmon", "-c", "1"]).output() {
        for l in String::from_utf8_lossy(&o.stdout).lines().filter(|l| !l.starts_with('#')) {
            let pid = l.split_whitespace().nth(1).and_then(|x| x.parse::<u32>().ok());
            if let Some((p, c)) = pid.and_then(|pid| procs.iter().find(|(q, _)| *q == pid)) {
                if !skip_proc(c) {
                    v.push((*p, short_name(c)));
                }
            }
        }
    }
    for (p, c) in &procs {
        if c.contains("steamapps/common/") && !skip_proc(c) {
            v.push((*p, short_name(c)));
        }
    }
    let mut seen = std::collections::HashSet::new();
    v.retain(|(_, n)| n != "steam" && seen.insert(n.clone()));
    v
}

/// The game to tune: an explicit PID, a name filter, or the first candidate.
fn find_game(pid: Option<u32>, filter: Option<&str>) -> Option<(u32, String)> {
    if let Some(pid) = pid {
        let cmd = String::from_utf8_lossy(&fs::read(format!("/proc/{pid}/cmdline")).ok()?).replace('\0', " ");
        return Some((pid, short_name(cmd.trim())));
    }
    if let Some(f) = filter {
        let f = f.to_lowercase();
        return processes().into_iter().find(|(p, c)| c.to_lowercase().contains(&f) && *p != std::process::id())
            .map(|(p, c)| (p, short_name(&c)));
    }
    candidates().into_iter().next()
}

fn load_profiles() -> HashMap<String, Limits> {
    let mut m = HashMap::new();
    for l in fs::read_to_string(PROFILES).unwrap_or_default().lines() {
        if let Some((name, v)) = l.rsplit_once('=') {
            let n: Vec<f64> = v.split(',').filter_map(|x| x.parse().ok()).collect();
            if n.len() == 3 {
                m.insert(name.to_string(), Limits { pl1: n[0], pl2: n[1], gpu: n[2] });
            }
        }
    }
    m
}

fn save_profile(name: &str, l: Limits) -> Result<(), String> {
    let mut m = load_profiles();
    m.insert(name.to_string(), l);
    let mut text: Vec<String> = m.iter().map(|(k, v)| format!("{k}={},{},{}", v.pl1, v.pl2, v.gpu)).collect();
    text.sort();
    fs::create_dir_all("/etc/powerlimit").map_err(|e| e.to_string())?;
    fs::write(PROFILES, text.join("\n") + "\n").map_err(|e| e.to_string())
}

fn forget(name: &str) -> Result<(), String> {
    let mut m = load_profiles();
    m.remove(name).ok_or(format!("no profile '{name}'"))?;
    let mut text: Vec<String> = m.iter().map(|(k, v)| format!("{k}={},{},{}", v.pl1, v.pl2, v.gpu)).collect();
    text.sort();
    fs::write(PROFILES, text.join("\n") + "\n").map_err(|e| e.to_string())
}

fn alive(pid: u32) -> bool {
    fs::metadata(format!("/proc/{pid}")).is_ok()
}

fn sleep_checked(secs: u64) {
    for _ in 0..secs {
        if STOP.load(Ordering::SeqCst) {
            return;
        }
        sleep(Duration::from_secs(1));
    }
}

struct Opts {
    pid: Option<u32>,
    game: Option<String>,
    minutes: u64,
    cpu_max: f64,
    gpu_max: f64,
    stdin_stop: bool, // GUI parent writes "stop" to our stdin
}

const K_MIN: u32 = 15;
const K_MAX: u32 = 50;
const MAX_TRIALS: u64 = 10;
const SPLIT_STEP: i32 = 3;

fn gpu_base(k: u32) -> f64 {
    (15.0 + (k as f64 - 15.0) * 30.0 / 35.0).round()
}

/// k = CPU sustained watts at an even split (GPU follows on a linear 15..45 W scale, burst is +10 W);
/// s shifts watts from CPU to GPU (negative: GPU to CPU), keeping the shared total roughly constant.
fn limits_for(k: u32, s: i32) -> Limits {
    let pl1 = (k as f64 - s as f64).max(10.0);
    Limits { pl1, pl2: (pl1 + 10.0).min(109.0), gpu: (gpu_base(k) + s as f64).clamp(10.0, 45.0) }
}

struct Trial {
    lim: Limits,
    cmhz: f64,
    gmhz: f64,
    util: f64, // mean GPU utilisation %, tells us which side is the bottleneck
    ok: bool,
}

enum Run {
    Done(Trial),
    Idle, // the game wasn't doing any work, so the numbers would say nothing
    Stopped,
}

/// CPU seconds a process has used so far (utime + stime).
fn proc_cpu_secs(pid: u32) -> Option<f64> {
    let st = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let f: Vec<&str> = st[st.rfind(')')? + 2..].split(' ').collect(); // fields after "(comm) "
    Some((f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / 100.0)
}

/// Clock weights by bottleneck: a GPU-bound game cares about GPU MHz, a CPU-bound one about CPU MHz.
fn weights(util: f64) -> (f64, f64) {
    let wg = if util >= 90.0 { 0.65 } else if util >= 60.0 { 0.5 } else { 0.35 };
    (1.0 - wg, wg)
}

/// Weighted clock score of every unthrottled trial, each side normalised to the best seen.
fn scored<'a>(trials: &'a [Trial], util: f64) -> Vec<(&'a Trial, f64)> {
    let ok: Vec<&Trial> = trials.iter().filter(|t| t.ok).collect();
    let (max_c, max_g) = ok.iter().fold((1.0f64, 1.0f64), |m, t| (m.0.max(t.cmhz), m.1.max(t.gmhz)));
    let (wc, wg) = weights(util);
    ok.into_iter()
        .map(|t| (t, wc * t.cmhz / max_c + if max_g > 1.0 { wg * t.gmhz / max_g } else { wg }))
        .collect()
}

fn best_score(trials: &[Trial], util: f64) -> f64 {
    scored(trials, util).iter().map(|x| x.1).fold(0.0, f64::max)
}

/// Apply one setting, let it heat-soak until temperatures level off, then measure.
fn run_trial(n: u64, lim: Limits, pid: u32, slice: u64, o: &Opts) -> Result<Run, String> {
    apply(lim)?;
    let warmup = slice * 2 / 5;
    say!("\nTrial {n}/{MAX_TRIALS}: CPU {}/{} W, GPU {} W - settling...", lim.pl1, lim.pl2, lim.gpu);
    sleep_checked(warmup);
    let mut extra = 0; // keep waiting while temps still climb > 1°C per 30 s
    while extra < slice / 4 && !STOP.load(Ordering::SeqCst) {
        let t = |s: &Sample| s.cpu_t.max(s.gpu_t);
        let before = t(&sample());
        sleep_checked(30);
        if (t(&sample()) - before).abs() < 1.0 {
            break;
        }
        extra += 30;
    }
    let (mut n_s, mut active, mut ct, mut gt, mut gw, mut gu, mut cw) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut hot, mut cmhz, mut gmhz) = (false, 0.0f64, 0.0f64);
    let thr0 = cpu_throttle_count();
    let (e0, t0) = (energy_uj(), Instant::now());
    let end = Instant::now() + Duration::from_secs(slice - warmup);
    let mut last_cpu = proc_cpu_secs(pid);
    while Instant::now() < end && !STOP.load(Ordering::SeqCst) {
        if !alive(pid) {
            say!("Game exited - ending early.");
            return Ok(Run::Stopped);
        }
        sleep_checked(5);
        let s = sample();
        let cpu_now = proc_cpu_secs(pid);
        let busy = cpu_now.zip(last_cpu).map(|(a, b)| (a - b) / 5.0).unwrap_or(0.0); // cores used by the game
        last_cpu = cpu_now;
        n_s += 1.0;
        if s.gpu_util >= 20.0 || busy >= 0.3 {
            active += 1.0;
        }
        ct = ct.max(s.cpu_t); // peak temps matter, not averages
        gt = gt.max(s.gpu_t);
        gw += s.gpu_w;
        gu += s.gpu_util;
        hot |= s.gpu_hot;
        cmhz += cpu_mhz();
        gmhz += s.gpu_mhz;
    }
    if STOP.load(Ordering::SeqCst) {
        return Ok(Run::Stopped);
    }
    if active < n_s / 2.0 {
        say!("  the game looks idle (menu or paused) - measurement discarded.");
        return Ok(Run::Idle);
    }
    if let (Some(a), Some(b)) = (e0, energy_uj()) {
        cw = (b - a).max(0.0) / 1e6 / t0.elapsed().as_secs_f64();
    }
    let (cmhz, gmhz, util) = (cmhz / n_s.max(1.0), gmhz / n_s.max(1.0), gu / n_s.max(1.0));
    let cpu_thr = cpu_throttle_count() > thr0;
    say!("  peak CPU {ct:.0}°C, peak GPU {gt:.0}°C | avg CPU {cw:.1} W / {cmhz:.0} MHz, GPU {:.1} W / {gmhz:.0} MHz, GPU util {util:.0}%{}{}",
        gw / n_s.max(1.0), if hot { " | GPU THERMAL THROTTLE" } else { "" }, if cpu_thr { " | CPU THERMAL THROTTLE" } else { "" });
    let ok = !(ct > o.cpu_max || gt > o.gpu_max || hot || cpu_thr);
    if !ok {
        say!("  throttling or over the temperature cap.");
    }
    Ok(Run::Done(Trial { lim, cmhz, gmhz, util, ok }))
}

/// Let the laptop cool so a hot trial doesn't contaminate the next one.
fn cooldown() {
    apply(limits_for(K_MIN, 0)).ok();
    say!("  cooling down...");
    for _ in 0..18 {
        let s = sample();
        if s.cpu_t <= 70.0 && s.gpu_t <= 65.0 || STOP.load(Ordering::SeqCst) {
            break;
        }
        sleep_checked(5);
    }
}

/// Run one trial, retrying once if the game was idle. Ok(None) = stopped or the game closed.
fn go(n: &mut u64, k: u32, s: i32, pid: u32, slice: u64, o: &Opts, trials: &mut Vec<Trial>) -> Result<Option<bool>, String> {
    for attempt in 0..2 {
        *n += 1;
        match run_trial(*n, limits_for(k, s), pid, slice, o)? {
            Run::Stopped => return Ok(None),
            Run::Idle if attempt == 0 => say!("  retrying - get into actual gameplay."),
            Run::Idle => return Err("the game never put the machine under load - start playing, then tune again".into()),
            Run::Done(t) => {
                let ok = t.ok;
                trials.push(t);
                if !ok {
                    cooldown();
                }
                return Ok(Some(ok));
            }
        }
    }
    Ok(None)
}

/// Three phases:
///  1. bisect the shared CPU+GPU budget in 1 W steps up to the thermal ceiling (throttle / temp caps);
///  2. at the ceiling, move watts toward the bottleneck (GPU-bound game: CPU -> GPU, else the reverse)
///     while the weighted clock score keeps improving - the two share one heatsink, so a watt is better
///     spent where it is used;
///  3. pick the knee: the cheapest unthrottled setting within 1.5% of the best score, so we don't burn
///     heat and fan noise for the last sliver of MHz.
fn tune(o: Opts) -> Result<(), String> {
    let (pid, name) = find_game(o.pid, o.game.as_deref()).ok_or("no game found - start the game first (or pick its process)")?;
    say!("Tuning for '{name}' (pid {pid}), {} min, caps: CPU {}°C / GPU {}°C", o.minutes, o.cpu_max, o.gpu_max);
    unsafe {
        signal(2, on_signal as *const () as usize);
        signal(15, on_signal as *const () as usize);
    }
    if o.stdin_stop {
        std::thread::spawn(|| {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if line.trim() == "stop" {
                    STOP.store(true, Ordering::SeqCst);
                }
            }
        });
    }
    let original = current();
    let slice = (o.minutes * 60 / MAX_TRIALS).max(60);
    let mut trials: Vec<Trial> = vec![];
    let (mut lo, mut hi) = (K_MIN - 1, K_MAX + 1); // highest known-good / lowest known-bad k (virtual until tested)
    let mut n = 0;
    let mut stopped = false;

    say!("Phase 1: finding the thermal ceiling");
    while hi - lo > 1 && n < MAX_TRIALS - 3 {
        let mid = (lo + hi) / 2;
        match go(&mut n, mid, 0, pid, slice, &o, &mut trials) {
            Ok(Some(true)) => lo = mid,
            Ok(Some(false)) => hi = mid,
            Ok(None) => { stopped = true; break; }
            Err(e) => { apply(original).ok(); return Err(e); }
        }
    }

    // Phase 2: shift watts toward the bottleneck
    let base_util = trials.iter().rev().find(|t| t.ok).map(|t| t.util);
    if let (false, Some(util)) = (stopped, base_util) {
        let dir = if util >= 90.0 { 1 } else { -1 };
        say!("\nPhase 2: GPU util {util:.0}% -> {} - moving watts {}", if dir > 0 { "GPU-bound" } else { "CPU-bound" },
            if dir > 0 { "from CPU to GPU" } else { "from GPU to CPU" });
        let mut s = 0;
        while n < MAX_TRIALS - 1 {
            let before = best_score(&trials, util);
            let next = s + dir * SPLIT_STEP;
            match go(&mut n, lo, next, pid, slice, &o, &mut trials) {
                Ok(Some(true)) if best_score(&trials, util) > before + 1e-3 => s = next,
                Ok(Some(_)) => break,
                Ok(None) => { stopped = true; break; }
                Err(e) => { apply(original).ok(); return Err(e); }
            }
        }
    }

    if stopped && trials.iter().all(|t| !t.ok) {
        say!("\nStopped before any setting passed - restoring previous limits, nothing saved.");
        return apply(original);
    }
    let util = base_util.unwrap_or(0.0);
    let results = scored(&trials, util);
    let top = results.iter().map(|x| x.1).fold(0.0, f64::max);
    say!("\nResults (no throttling; weights CPU {:.0}% / GPU {:.0}%):", weights(util).0 * 100.0, weights(util).1 * 100.0);
    for (t, sc) in &results {
        say!("  CPU {}/{} W, GPU {} W -> {:.0} MHz CPU, {:.0} MHz GPU (score {:.3})", t.lim.pl1, t.lim.pl2, t.lim.gpu, t.cmhz, t.gmhz, sc);
    }
    let pick = results.iter().filter(|x| x.1 >= top * 0.985)
        .min_by(|a, b| (a.0.lim.pl1 + a.0.lim.gpu).partial_cmp(&(b.0.lim.pl1 + b.0.lim.gpu)).unwrap())
        .map(|x| x.0.lim).unwrap_or_else(|| limits_for(K_MIN, 0));
    apply(pick)?;
    save_profile(&name, pick)?;
    say!("\nBest for '{name}': CPU {}/{} W, GPU {} W - applied and saved to {PROFILES}", pick.pl1, pick.pl2, pick.gpu);
    Ok(())
}

/// Apply a saved profile while its game runs; reset when no known game is running.
fn watch(stdin_stop: bool) -> Result<(), String> {
    if stdin_stop {
        std::thread::spawn(|| {
            for line in std::io::stdin().lock().lines().map_while(Result::ok) {
                if line.trim() == "stop" {
                    STOP.store(true, Ordering::SeqCst);
                }
            }
        });
    }
    unsafe {
        signal(2, on_signal as *const () as usize);
        signal(15, on_signal as *const () as usize);
    }
    let mut active: Option<String> = None;
    say!("Watching for tuned games...");
    while !STOP.load(Ordering::SeqCst) {
        let profiles = load_profiles();
        let running = processes().into_iter().map(|(_, c)| short_name(&c)).find(|n| profiles.contains_key(n));
        if running != active {
            match &running {
                Some(n) => { say!("{n} started - applying profile"); apply(profiles[n])?; }
                None => { say!("game closed - restoring defaults"); apply(DEFAULT)?; }
            }
            active = running;
        }
        sleep_checked(5);
    }
    apply(DEFAULT)
}

fn status() {
    let l = current();
    let s = sample();
    println!("CPU limits: {}/{} W | GPU limit: {} W", l.pl1, l.pl2, l.gpu);
    println!("CPU {:.0}°C | GPU {:.0}°C, {:.1} W, {:.0}% util", s.cpu_t, s.gpu_t, s.gpu_w, s.gpu_util);
    for (k, v) in load_profiles() {
        println!("profile {k}: {}/{} W, GPU {} W", v.pl1, v.pl2, v.gpu);
    }
}


fn usage() {
    println!("powerlimit - XPS 15 power limiter/tuner

  status                         current limits, temps, saved profiles
  procs                          list running game candidates (pid, name)
  set <cpuPL1> <cpuPL2> <gpuW>   e.g. set 35 45 30         (root)
  reset                          Dell defaults             (root)
  tune [--pid PID | --game NAME] [--minutes 30] [--cpu-max 85] [--gpu-max 80]   (root)
                                 run while a game is open; finds best limits and saves them
  gui                            graphical window (default when started from the app menu)
  forget <game>                  delete a saved profile    (root)
  watch                          auto-apply saved profiles when their game runs (root)

Run root commands with sudo. Limits reset on reboot.");
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let cmd = a.first().map(String::as_str).unwrap_or("");
    if matches!(cmd, "set" | "reset" | "tune" | "watch" | "forget") && !is_root() {
        eprintln!("needs root: run with sudo");
        std::process::exit(1);
    }
    let res = match cmd {
        "status" => { status(); Ok(()) }
        "procs" => { for (p, n) in candidates() { println!("{p}\t{n}"); } Ok(()) }
        "reset" => apply(DEFAULT),
        "set" => match a[1..].iter().map(|x| x.parse::<f64>()).collect::<Result<Vec<_>, _>>() {
            Ok(n) if n.len() == 3 && n[0] >= 10.0 && n[1] >= n[0] && n[1] <= 109.0 && (5.0..=45.0).contains(&n[2]) =>
                apply(Limits { pl1: n[0], pl2: n[1], gpu: n[2] }),
            Ok(_) => Err("out of range: need 10 <= PL1 <= PL2 <= 109 and 5 <= GPU <= 45".into()),
            _ => Err("usage: set <cpuPL1> <cpuPL2> <gpuW>".into()),
        },
        "tune" => {
            let mut o = Opts { pid: None, game: None, minutes: 30, cpu_max: 85.0, gpu_max: 80.0, stdin_stop: false };
            let mut it = a[1..].iter();
            while let Some(flag) = it.next() {
                if flag == "--stdin-stop" { o.stdin_stop = true; continue; }
                let v = it.next().cloned().unwrap_or_default();
                match flag.as_str() {
                    "--game" => o.game = Some(v),
                    "--pid" => o.pid = v.parse().ok(),
                    "--minutes" => o.minutes = v.parse().unwrap_or(30),
                    "--cpu-max" => o.cpu_max = v.parse().unwrap_or(85.0),
                    "--gpu-max" => o.gpu_max = v.parse().unwrap_or(80.0),
                    _ => {}
                }
            }
            tune(o)
        }
        "watch" => watch(a.iter().any(|x| x == "--stdin-stop")),
        "forget" => forget(a.get(1).map(String::as_str).unwrap_or("")),
        "gui" => { gui::run(); Ok(()) }
        "" => { gui::run(); Ok(()) }
        _ => { usage(); Ok(()) }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
