//! Hardware detection. Each probe is a parser over a command's output or a
//! sysfs tree, so tests feed fixtures and never depend on the host.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::util::MIB;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Cpu {
    pub brand: String,
    pub physical_cores: usize,
    pub logical_cores: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Gpu {
    /// nvidia, amd, intel, apple
    pub vendor: String,
    pub name: String,
    /// Dedicated memory in bytes. For unified memory this is the system RAM.
    pub vram_total: Option<u64>,
    pub vram_free: Option<u64>,
    /// metal, cuda, rocm, vulkan, cpu
    pub backend: String,
    /// Shares system RAM (Intel iGPU, AMD APU, Apple Silicon).
    pub integrated: bool,
    pub driver: Option<String>,
    /// Which probe found it.
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hardware {
    pub os: String,
    pub os_version: Option<String>,
    pub arch: String,
    pub cpu: Cpu,
    pub ram_total: u64,
    pub ram_available: u64,
    pub gpus: Vec<Gpu>,
    pub unified_memory: bool,
    /// The backend Fonsviva will run models on: metal, cuda, vulkan or cpu.
    pub backend: String,
    /// Plain-words notes about what was and was not found.
    pub notes: Vec<String>,
}

impl Hardware {
    /// The discrete GPU with the most memory, if it is worth offloading to.
    pub fn best_gpu(&self) -> Option<&Gpu> {
        self.gpus
            .iter()
            .filter(|g| !g.integrated && g.vram_total.unwrap_or(0) >= 2 * crate::util::GIB)
            .max_by_key(|g| g.vram_total.unwrap_or(0))
    }
}

/// Runs a probe command with a timeout; None when it is missing, fails or hangs.
pub fn run_probe(cmd: &str, args: &[&str]) -> Option<String> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                use std::io::Read;
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return status.success().then_some(out);
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Detects this machine. Blocking; call from a blocking task.
pub fn detect() -> Hardware {
    use sysinfo::System;
    let mut notes = vec![];
    let (cpu, ram_total, ram_available) = match System::new() {
        Ok(mut sys) => {
            sys.refresh_memory();
            sys.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
            let brand = sys
                .cpus()
                .first()
                .map(|c| c.brand().trim().to_string())
                .unwrap_or_default();
            let cpu = Cpu {
                brand,
                physical_cores: System::physical_core_count().unwrap_or(0),
                logical_cores: sys.cpus().len(),
            };
            (cpu, sys.total_memory(), sys.available_memory())
        }
        Err(e) => {
            notes.push(format!("Could not read CPU and memory: {e}"));
            let cpu = Cpu {
                brand: String::new(),
                physical_cores: 0,
                logical_cores: std::thread::available_parallelism().map_or(0, |n| n.get()),
            };
            (cpu, 0, 0)
        }
    };
    let os = std::env::consts::OS.to_string();
    let arch = std::env::consts::ARCH.to_string();
    let mut gpus = vec![];

    if let Some(out) = run_probe(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,memory.free,driver_version",
            "--format=csv,noheader,nounits",
        ],
    ) {
        gpus.extend(parse_nvidia_smi(&out));
    }

    match os.as_str() {
        "macos" => {
            let out = run_probe("sysctl", &["-n", "machdep.cpu.brand_string", "hw.memsize"]);
            if let Some(g) = out.as_deref().and_then(|o| parse_sysctl(o, &arch)) {
                gpus.push(g);
            }
        }
        "linux" => {
            let vk = run_probe("vulkaninfo", &["--summary"])
                .map(|o| parse_vulkaninfo(&o))
                .unwrap_or_default();
            let loader = vulkan_loader_present();
            for g in parse_sysfs_drm(Path::new("/sys/class/drm"), loader, &vk) {
                if g.vendor == "nvidia" && gpus.iter().any(|n| n.vendor == "nvidia") {
                    continue;
                }
                gpus.push(g);
            }
            if !loader && gpus.iter().any(|g| g.vendor != "nvidia") {
                notes.push("No Vulkan loader (libvulkan.so.1) found, so AMD and Intel GPUs can't be used. Install your distribution's Vulkan package to use them.".into());
            }
        }
        "windows" => {
            let script = "Get-ItemProperty 'HKLM:\\SYSTEM\\ControlSet001\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}\\0*' -ErrorAction SilentlyContinue | Select-Object DriverDesc,ProviderName,DriverVersion,'HardwareInformation.qwMemorySize',MatchingDeviceId | ConvertTo-Json";
            if let Some(out) = run_probe(
                "powershell",
                &["-NoProfile", "-NonInteractive", "-Command", script],
            ) {
                for g in parse_windows_adapters(&out) {
                    if g.vendor == "nvidia" && gpus.iter().any(|n| n.vendor == "nvidia") {
                        continue;
                    }
                    gpus.push(g);
                }
            }
        }
        _ => {}
    }

    let unified_memory = gpus.iter().any(|g| g.vendor == "apple");
    let mut hw = Hardware {
        os_version: sysinfo::System::long_os_version().ok(),
        os,
        arch,
        cpu,
        ram_total,
        ram_available,
        gpus,
        unified_memory,
        backend: "cpu".into(),
        notes,
    };
    hw.backend = choose_backend(&hw);
    if hw.backend == "cpu" {
        if hw.gpus.is_empty() {
            hw.notes
                .push("No usable GPU found. Models run on the CPU, which is slower.".into());
        } else {
            hw.notes.push(
                "The GPU here is integrated or too small to help, so models run on the CPU.".into(),
            );
        }
    }
    hw
}

/// The backend Fonsviva runs on: Metal on Apple Silicon, CUDA for NVIDIA,
/// Vulkan for other discrete GPUs, else CPU.
pub fn choose_backend(hw: &Hardware) -> String {
    if hw.unified_memory {
        return "metal".into();
    }
    match hw.best_gpu() {
        Some(g) if g.vendor == "nvidia" => "cuda".into(),
        Some(g) if g.backend != "cpu" => "vulkan".into(),
        _ => "cpu".into(),
    }
}

pub fn parse_nvidia_smi(out: &str) -> Vec<Gpu> {
    out.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            if f.len() < 3 {
                return None;
            }
            let mib = |s: &str| s.parse::<u64>().ok().map(|v| v * MIB);
            Some(Gpu {
                vendor: "nvidia".into(),
                name: f[0].to_string(),
                vram_total: mib(f[1]),
                vram_free: mib(f[2]),
                backend: "cuda".into(),
                integrated: false,
                driver: f.get(3).map(|s| s.to_string()),
                source: "nvidia-smi".into(),
            })
        })
        .collect()
}

/// `sysctl -n machdep.cpu.brand_string hw.memsize` on a Mac.
pub fn parse_sysctl(out: &str, arch: &str) -> Option<Gpu> {
    let mut lines = out.lines().map(str::trim);
    let brand = lines.next()?.to_string();
    let mem: u64 = lines.next()?.parse().ok()?;
    if arch != "aarch64" || !brand.starts_with("Apple") {
        return None;
    }
    Some(Gpu {
        vendor: "apple".into(),
        name: format!("{brand} GPU"),
        vram_total: Some(mem),
        vram_free: None,
        backend: "metal".into(),
        integrated: true,
        driver: None,
        source: "sysctl".into(),
    })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct VulkanDevice {
    pub name: String,
    pub vendor_id: u32,
    pub kind: String,
}

/// `vulkaninfo --summary`, one entry per GPUn block.
pub fn parse_vulkaninfo(out: &str) -> Vec<VulkanDevice> {
    let mut devs = vec![];
    let mut cur: Option<VulkanDevice> = None;
    for line in out.lines() {
        let t = line.trim();
        if t.starts_with("GPU") && t.ends_with(':') {
            devs.extend(cur.take());
            cur = Some(VulkanDevice::default());
            continue;
        }
        let Some(d) = cur.as_mut() else { continue };
        let Some((k, v)) = t.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "deviceName" => d.name = v.to_string(),
            "vendorID" => {
                d.vendor_id = u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0)
            }
            "deviceType" => d.kind = v.to_string(),
            _ => {}
        }
    }
    devs.extend(cur);
    devs.retain(|d| !d.kind.contains("CPU"));
    devs
}

fn vulkan_loader_present() -> bool {
    [
        "/usr/lib/libvulkan.so.1",
        "/usr/lib64/libvulkan.so.1",
        "/usr/lib/x86_64-linux-gnu/libvulkan.so.1",
        "/usr/lib/aarch64-linux-gnu/libvulkan.so.1",
    ]
    .iter()
    .any(|p| Path::new(p).exists())
}

fn vendor_name(id: u32) -> Option<&'static str> {
    match id {
        0x10de => Some("nvidia"),
        0x1002 => Some("amd"),
        0x8086 => Some("intel"),
        _ => None,
    }
}

/// Reads `/sys/class/drm/cardN/device`: vendor, device and amdgpu's VRAM counters.
pub fn parse_sysfs_drm(root: &Path, vulkan_loader: bool, vk: &[VulkanDevice]) -> Vec<Gpu> {
    let mut cards: Vec<_> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_prefix("card")
                .is_some_and(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
        })
        .map(|e| e.path())
        .collect();
    cards.sort();
    let read = |p: &Path, f: &str| {
        std::fs::read_to_string(p.join(f))
            .ok()
            .map(|s| s.trim().to_string())
    };
    let mut out = vec![];
    for card in cards {
        let dev = card.join("device");
        let Some(vid) = read(&dev, "vendor")
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        else {
            continue;
        };
        let Some(vendor) = vendor_name(vid) else {
            continue;
        };
        let device_id = read(&dev, "device").unwrap_or_default();
        let vram_total = read(&dev, "mem_info_vram_total").and_then(|v| v.parse::<u64>().ok());
        let vram_used = read(&dev, "mem_info_vram_used").and_then(|v| v.parse::<u64>().ok());
        let vk_dev = vk.iter().find(|d| d.vendor_id == vid);
        let name = vk_dev
            .map(|d| d.name.clone())
            .or_else(|| read(&dev, "product_name"))
            .unwrap_or_else(|| format!("{} GPU {device_id}", vendor.to_uppercase()));
        let integrated = match vk_dev {
            Some(d) => d.kind.contains("INTEGRATED"),
            None => vendor == "intel" || vram_total.unwrap_or(0) < 2 * crate::util::GIB,
        };
        let backend = if vulkan_loader || vk_dev.is_some() {
            "vulkan"
        } else {
            "cpu"
        };
        out.push(Gpu {
            vendor: vendor.into(),
            name,
            vram_total: if integrated { None } else { vram_total },
            vram_free: match (vram_total, vram_used) {
                (Some(t), Some(u)) if !integrated => Some(t.saturating_sub(u)),
                _ => None,
            },
            backend: backend.into(),
            integrated,
            driver: None,
            source: "sysfs".into(),
        });
    }
    out
}

/// Display adapters from the Windows registry (the same numbers DXGI reports
/// as dedicated video memory), via one PowerShell call.
pub fn parse_windows_adapters(json: &str) -> Vec<Gpu> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let items = match v {
        serde_json::Value::Array(a) => a,
        o @ serde_json::Value::Object(_) => vec![o],
        _ => vec![],
    };
    items
        .iter()
        .filter_map(|a| {
            let name = a.get("DriverDesc")?.as_str()?.to_string();
            let id = a
                .get("MatchingDeviceId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_lowercase();
            let vendor = if id.contains("ven_10de") {
                "nvidia"
            } else if id.contains("ven_1002") {
                "amd"
            } else if id.contains("ven_8086") {
                "intel"
            } else {
                return None;
            };
            let mem = a
                .get("HardwareInformation.qwMemorySize")
                .and_then(|v| v.as_u64());
            let integrated = vendor == "intel" || mem.unwrap_or(0) < 2 * crate::util::GIB;
            Some(Gpu {
                vendor: vendor.into(),
                name,
                vram_total: if integrated { None } else { mem },
                vram_free: None,
                backend: if vendor == "nvidia" { "cuda" } else { "vulkan" }.into(),
                integrated,
                driver: a
                    .get("DriverVersion")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                source: "registry".into(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::GIB;

    fn hw(os: &str, gpus: Vec<Gpu>) -> Hardware {
        Hardware {
            os: os.into(),
            os_version: None,
            arch: "x86_64".into(),
            cpu: Cpu {
                brand: "x".into(),
                physical_cores: 8,
                logical_cores: 16,
            },
            ram_total: 32 * GIB,
            ram_available: 20 * GIB,
            unified_memory: gpus.iter().any(|g| g.vendor == "apple"),
            gpus,
            backend: String::new(),
            notes: vec![],
        }
    }

    #[test]
    fn nvidia_smi_fixture() {
        let out = "NVIDIA GeForce RTX 4080, 16376, 15900, 580.82.07\nNVIDIA GeForce RTX 3060, 12288, 100, 580.82.07\n";
        let g = parse_nvidia_smi(out);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name, "NVIDIA GeForce RTX 4080");
        assert_eq!(g[0].vram_total, Some(16376 * MIB));
        assert_eq!(g[0].vram_free, Some(15900 * MIB));
        assert_eq!(g[0].driver.as_deref(), Some("580.82.07"));
        assert_eq!(choose_backend(&hw("windows", g.clone())), "cuda");
        assert_eq!(choose_backend(&hw("linux", g)), "cuda");
        assert!(parse_nvidia_smi("NVIDIA-SMI has failed").is_empty());
    }

    #[test]
    fn sysctl_fixture() {
        let g = parse_sysctl("Apple M3 Pro\n38654705664\n", "aarch64").unwrap();
        assert_eq!(g.vendor, "apple");
        assert_eq!(g.backend, "metal");
        assert_eq!(g.vram_total, Some(36 * GIB));
        assert!(parse_sysctl("Intel(R) Core(TM) i9\n17179869184\n", "x86_64").is_none());
        assert_eq!(choose_backend(&hw("macos", vec![g])), "metal");
    }

    const VULKANINFO: &str = "\
Devices:
========
GPU0:
\tapiVersion         = 1.4.309
\tvendorID           = 0x1002
\tdeviceID           = 0x744c
\tdeviceType         = PHYSICAL_DEVICE_TYPE_DISCRETE_GPU
\tdeviceName         = AMD Radeon RX 7900 XTX (RADV NAVI31)
GPU1:
\tvendorID           = 0x10005
\tdeviceType         = PHYSICAL_DEVICE_TYPE_CPU
\tdeviceName         = llvmpipe (LLVM 19.1.7, 256 bits)
";

    #[test]
    fn vulkaninfo_fixture() {
        let d = parse_vulkaninfo(VULKANINFO);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].vendor_id, 0x1002);
        assert!(d[0].name.starts_with("AMD Radeon RX 7900 XTX"));
    }

    #[test]
    fn sysfs_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let mk = |card: &str, files: &[(&str, &str)]| {
            let d = dir.path().join(card).join("device");
            std::fs::create_dir_all(&d).unwrap();
            for (f, v) in files {
                std::fs::write(d.join(f), v).unwrap();
            }
        };
        mk(
            "card0",
            &[
                ("vendor", "0x1002\n"),
                ("device", "0x744c\n"),
                ("mem_info_vram_total", "25753026560\n"),
                ("mem_info_vram_used", "753026560\n"),
            ],
        );
        mk("card1", &[("vendor", "0x8086\n"), ("device", "0x9b41\n")]);
        // Connector entries are not cards.
        mk("card1-eDP-1", &[("vendor", "0x8086\n")]);

        let vk = parse_vulkaninfo(VULKANINFO);
        let g = parse_sysfs_drm(dir.path(), true, &vk);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].vendor, "amd");
        assert!(g[0].name.contains("7900 XTX"));
        assert_eq!(g[0].vram_total, Some(25753026560));
        assert_eq!(g[0].vram_free, Some(25_000_000_000));
        assert!(!g[0].integrated);
        assert!(g[1].integrated);
        assert_eq!(choose_backend(&hw("linux", g.clone())), "vulkan");

        // Intel iGPU alone (chapel1's case): honest CPU.
        let only_igpu = parse_sysfs_drm(dir.path(), true, &[])
            .into_iter()
            .filter(|g| g.vendor == "intel")
            .collect();
        assert_eq!(choose_backend(&hw("linux", only_igpu)), "cpu");

        // No Vulkan loader: the AMD card can't be used.
        let g = parse_sysfs_drm(dir.path(), false, &[]);
        assert_eq!(g[0].backend, "cpu");
        assert_eq!(choose_backend(&hw("linux", g)), "cpu");
    }

    #[test]
    fn windows_registry_fixture() {
        let json = r#"[
          {"DriverDesc":"NVIDIA GeForce RTX 4070","ProviderName":"NVIDIA","DriverVersion":"32.0.15.8180",
           "HardwareInformation.qwMemorySize":12878610432,"MatchingDeviceId":"PCI\\VEN_10DE&DEV_2786"},
          {"DriverDesc":"Intel(R) UHD Graphics 770","DriverVersion":"31.0.101.5186",
           "HardwareInformation.qwMemorySize":134217728,"MatchingDeviceId":"PCI\\VEN_8086&DEV_4680"},
          {"DriverDesc":"Microsoft Basic Display Adapter","MatchingDeviceId":"ROOT\\BasicDisplay"}
        ]"#;
        let g = parse_windows_adapters(json);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].vram_total, Some(12878610432));
        assert!(g[1].integrated);
        assert_eq!(choose_backend(&hw("windows", g)), "cuda");
        // A single adapter comes back as an object, not an array.
        let one = r#"{"DriverDesc":"AMD Radeon RX 6600","HardwareInformation.qwMemorySize":8573157376,"MatchingDeviceId":"PCI\\VEN_1002&DEV_73FF"}"#;
        assert_eq!(parse_windows_adapters(one)[0].vendor, "amd");
    }

    #[test]
    fn no_gpu_is_cpu() {
        assert_eq!(choose_backend(&hw("linux", vec![])), "cpu");
    }
}
