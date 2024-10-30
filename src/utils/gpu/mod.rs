mod amd;
mod intel;
mod nvidia;
mod other;

use anyhow::{bail, Context, Result};
use log::{debug, info};
use process_data::pci_slot::PciSlot;

use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

use glob::glob;

use crate::{
    i18n::i18n,
    utils::{pci::Device, read_uevent},
};

use self::{amd::AmdGpu, intel::IntelGpu, nvidia::NvidiaGpu, other::OtherGpu};

use super::pci::Vendor;

pub const VID_AMD: u16 = 4098;
pub const VID_INTEL: u16 = 32902;
pub const VID_NVIDIA: u16 = 4318;

#[derive(Debug)]
pub struct GpuData {
    pub pci_slot: PciSlot,

    pub usage_fraction: Option<f64>,

    // in case of a GPU with a combined media engine, encode_fraction will contain the combined usage
    pub encode_fraction: Option<f64>,
    pub decode_fraction: Option<f64>,

    pub total_vram: Option<usize>,
    pub used_vram: Option<usize>,

    pub clock_speed: Option<f64>,
    pub vram_speed: Option<f64>,

    pub temperature: Option<f64>,

    pub power_usage: Option<f64>,
    pub power_cap: Option<f64>,
    pub power_cap_max: Option<f64>,

    pub nvidia: bool,
}

impl GpuData {
    pub fn new(gpu: &Gpu) -> Self {
        let pci_slot = gpu.pci_slot();

        let usage_fraction = gpu
            .usage()
            .map(|usage| (usage / 100.0).clamp(0.0, 1.0))
            .ok();

        let encode_fraction = gpu
            .encode_usage()
            .map(|usage| (usage / 100.0).clamp(0.0, 1.0))
            .ok();

        let decode_fraction = gpu
            .decode_usage()
            .map(|usage| (usage / 100.0).clamp(0.0, 1.0))
            .ok();

        let total_vram = gpu.total_vram().ok();
        let used_vram = gpu.used_vram().ok();

        let clock_speed = gpu.core_frequency().ok();
        let vram_speed = gpu.vram_frequency().ok();

        let temperature = gpu.temperature().ok();

        let power_usage = gpu.power_usage().ok();
        let power_cap = gpu.power_cap().ok();
        let power_cap_max = gpu.power_cap_max().ok();

        let nvidia = matches!(gpu, Gpu::Nvidia(_));

        Self {
            pci_slot,
            usage_fraction,
            encode_fraction,
            decode_fraction,
            total_vram,
            used_vram,
            clock_speed,
            vram_speed,
            temperature,
            power_usage,
            power_cap,
            power_cap_max,
            nvidia,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Gpu {
    Amd(AmdGpu),
    Nvidia(NvidiaGpu),
    Intel(IntelGpu),
    Other(OtherGpu),
}

impl Default for Gpu {
    fn default() -> Self {
        Gpu::Other(OtherGpu::default())
    }
}

pub trait GpuImpl {
    fn device(&self) -> Option<&'static Device>;
    fn pci_slot(&self) -> PciSlot;
    fn driver(&self) -> String;
    fn sysfs_path(&self) -> PathBuf;
    fn first_hwmon(&self) -> Option<PathBuf>;

    fn name(&self) -> Result<String>;
    fn usage(&self) -> Result<f64>;
    fn encode_usage(&self) -> Result<f64>;
    fn decode_usage(&self) -> Result<f64>;
    fn combined_media_engine(&self) -> Result<bool>;
    fn used_vram(&self) -> Result<usize>;
    fn total_vram(&self) -> Result<usize>;
    fn temperature(&self) -> Result<f64>;
    fn power_usage(&self) -> Result<f64>;
    fn core_frequency(&self) -> Result<f64>;
    fn vram_frequency(&self) -> Result<f64>;
    fn power_cap(&self) -> Result<f64>;
    fn power_cap_max(&self) -> Result<f64>;

    fn read_sysfs_int<P: AsRef<Path> + std::marker::Send>(&self, file: P) -> Result<isize> {
        let path = self.sysfs_path().join(file);
        std::fs::read_to_string(&path)?
            .replace('\n', "")
            .parse::<isize>()
            .with_context(|| format!("error parsing file {}", &path.to_string_lossy()))
    }

    fn read_device_file<P: AsRef<Path> + std::marker::Send>(&self, file: P) -> Result<String> {
        let path = self.sysfs_path().join("device").join(file);
        Ok(std::fs::read_to_string(path)?.replace('\n', ""))
    }

    fn read_device_int<P: AsRef<Path> + std::marker::Send>(&self, file: P) -> Result<isize> {
        let path = self.sysfs_path().join("device").join(file);
        self.read_device_file(&path)?
            .parse::<isize>()
            .with_context(|| format!("error parsing file {}", &path.to_string_lossy()))
    }

    fn read_hwmon_int<P: AsRef<Path> + std::marker::Send>(&self, file: P) -> Result<isize> {
        let path = self.first_hwmon().context("no hwmon found")?.join(file);
        std::fs::read_to_string(&path)?
            .replace('\n', "")
            .parse::<isize>()
            .with_context(|| format!("error parsing file {}", &path.to_string_lossy()))
    }

    // These are preimplemented ways of getting information through the DRM and hwmon interface.
    // It's also used as a fallback.

    fn drm_name(&self) -> Result<String> {
        Ok(self.device().context("no device")?.name().to_owned())
    }

    fn drm_usage(&self) -> Result<isize> {
        self.read_device_int("gpu_busy_percent")
    }

    fn drm_used_vram(&self) -> Result<isize> {
        self.read_device_int("mem_info_vram_used")
    }

    fn drm_total_vram(&self) -> Result<isize> {
        self.read_device_int("mem_info_vram_total")
    }

    fn hwmon_temperature(&self) -> Result<f64> {
        Ok(self.read_hwmon_int("temp1_input")? as f64 / 1000.0)
    }

    fn hwmon_power_usage(&self) -> Result<f64> {
        Ok(self
            .read_hwmon_int("power1_average")
            .or_else(|_| self.read_hwmon_int("power1_input"))? as f64
            / 1_000_000.0)
    }

    fn hwmon_core_frequency(&self) -> Result<f64> {
        Ok(self.read_hwmon_int("freq1_input")? as f64)
    }

    fn hwmon_vram_frequency(&self) -> Result<f64> {
        Ok(self.read_hwmon_int("freq2_input")? as f64)
    }

    fn hwmon_power_cap(&self) -> Result<f64> {
        Ok(self.read_hwmon_int("power1_cap")? as f64 / 1_000_000.0)
    }

    fn hwmon_power_cap_max(&self) -> Result<f64> {
        Ok(self.read_hwmon_int("power1_cap_max")? as f64 / 1_000_000.0)
    }
}

impl Gpu {
    /// Returns a `Vec` of all GPUs currently found in the system.
    ///
    /// # Errors
    ///
    /// Will return `Err` if there are problems detecting
    /// the GPUs in the system
    pub fn get_gpus() -> Result<Vec<Gpu>> {
        debug!("Searching for GPUs…");

        let mut gpu_vec: Vec<Gpu> = Vec::new();
        for entry in glob("/sys/class/drm/card?")?.flatten() {
            if let Ok(gpu) = Self::from_sysfs_path(entry) {
                gpu_vec.push(gpu);
            }
        }

        debug!("{} GPUs found", gpu_vec.len());

        Ok(gpu_vec)
    }

    fn from_sysfs_path<P: AsRef<Path>>(path: P) -> Result<Gpu> {
        let sysfs_device_path = path.as_ref().join("device");
        let uevent_contents = read_uevent(sysfs_device_path.join("uevent"))?;

        let (device, vid, pid) = if let Some(pci_line) = uevent_contents.get("PCI_ID") {
            let (vid_str, pid_str) = pci_line.split_once(':').unwrap_or(("0", "0"));
            let vid = u16::from_str_radix(vid_str, 16).unwrap_or_default();
            let pid = u16::from_str_radix(pid_str, 16).unwrap_or_default();
            (Device::from_vid_pid(vid, pid), vid, pid)
        } else {
            (None, 0, 0)
        };

        let mut hwmon_vec: Vec<PathBuf> = Vec::new();
        for hwmon in glob(&format!(
            "{}/hwmon/hwmon?",
            sysfs_device_path
                .to_str()
                .context("error transforming PathBuf to str")?
        ))?
        .flatten()
        {
            hwmon_vec.push(hwmon);
        }

        let pci_slot = PciSlot::from_str(
            &uevent_contents
                .get("PCI_SLOT_NAME")
                .map_or_else(|| i18n("N/A"), std::string::ToString::to_string),
        )
        .context("can't turn PCI string to struct")?;

        let driver = uevent_contents
            .get("DRIVER")
            .map_or_else(|| i18n("N/A"), std::string::ToString::to_string);

        // if the driver is simple-framebuffer, it's likely not a GPU
        if driver == "simple-framebuffer" {
            bail!("this is a simple framebuffer");
        }

        let path = path.as_ref().to_path_buf();

        let (gpu, gpu_category) = if vid == VID_AMD || driver == "amdgpu" {
            (
                Gpu::Amd(AmdGpu::new(
                    device,
                    pci_slot,
                    driver,
                    path,
                    hwmon_vec.first().cloned(),
                )),
                "AMD",
            )
        } else if vid == VID_INTEL || driver == "i915" {
            (
                Gpu::Intel(IntelGpu::new(
                    device,
                    pci_slot,
                    driver,
                    path,
                    hwmon_vec.first().cloned(),
                )),
                "Intel",
            )
        } else if vid == VID_NVIDIA || driver == "nvidia" {
            (
                Gpu::Nvidia(NvidiaGpu::new(
                    device,
                    pci_slot,
                    driver,
                    path,
                    hwmon_vec.first().cloned(),
                )),
                "NVIDIA",
            )
        } else {
            (
                Gpu::Other(OtherGpu::new(
                    device,
                    pci_slot,
                    driver,
                    path,
                    hwmon_vec.first().cloned(),
                )),
                "Other",
            )
        };

        info!(
            "Found GPU \"{}\" (PCI slot: {} · PCI ID: {vid:x}:{pid:x} · Category: {gpu_category})",
            gpu.name().unwrap_or("<unknown name>".into()),
            gpu.pci_slot(),
        );

        Ok(gpu)
    }

    pub fn get_vendor(&self) -> Result<&'static Vendor> {
        Ok(match self {
            Gpu::Amd(gpu) => gpu.device(),
            Gpu::Nvidia(gpu) => gpu.device(),
            Gpu::Intel(gpu) => gpu.device(),
            Gpu::Other(gpu) => gpu.device(),
        }
        .context("no device")?
        .vendor())
    }

    pub fn pci_slot(&self) -> PciSlot {
        match self {
            Gpu::Amd(gpu) => gpu.pci_slot(),
            Gpu::Nvidia(gpu) => gpu.pci_slot(),
            Gpu::Intel(gpu) => gpu.pci_slot(),
            Gpu::Other(gpu) => gpu.pci_slot(),
        }
    }

    pub fn driver(&self) -> String {
        match self {
            Gpu::Amd(gpu) => gpu.driver(),
            Gpu::Nvidia(gpu) => gpu.driver(),
            Gpu::Intel(gpu) => gpu.driver(),
            Gpu::Other(gpu) => gpu.driver(),
        }
    }

    pub fn name(&self) -> Result<String> {
        match self {
            Gpu::Amd(gpu) => gpu.name(),
            Gpu::Nvidia(gpu) => gpu.name(),
            Gpu::Intel(gpu) => gpu.name(),
            Gpu::Other(gpu) => gpu.name(),
        }
    }

    pub fn usage(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.usage(),
            Gpu::Nvidia(gpu) => gpu.usage(),
            Gpu::Intel(gpu) => gpu.usage(),
            Gpu::Other(gpu) => gpu.usage(),
        }
    }

    pub fn encode_usage(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.encode_usage(),
            Gpu::Nvidia(gpu) => gpu.encode_usage(),
            Gpu::Intel(gpu) => gpu.encode_usage(),
            Gpu::Other(gpu) => gpu.encode_usage(),
        }
    }

    pub fn decode_usage(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.decode_usage(),
            Gpu::Nvidia(gpu) => gpu.decode_usage(),
            Gpu::Intel(gpu) => gpu.decode_usage(),
            Gpu::Other(gpu) => gpu.decode_usage(),
        }
    }

    pub fn combined_media_engine(&self) -> Result<bool> {
        match self {
            Gpu::Amd(gpu) => gpu.combined_media_engine(),
            Gpu::Nvidia(gpu) => gpu.combined_media_engine(),
            Gpu::Intel(gpu) => gpu.combined_media_engine(),
            Gpu::Other(gpu) => gpu.combined_media_engine(),
        }
    }

    pub fn used_vram(&self) -> Result<usize> {
        match self {
            Gpu::Amd(gpu) => gpu.used_vram(),
            Gpu::Nvidia(gpu) => gpu.used_vram(),
            Gpu::Intel(gpu) => gpu.used_vram(),
            Gpu::Other(gpu) => gpu.used_vram(),
        }
    }

    pub fn total_vram(&self) -> Result<usize> {
        match self {
            Gpu::Amd(gpu) => gpu.total_vram(),
            Gpu::Nvidia(gpu) => gpu.total_vram(),
            Gpu::Intel(gpu) => gpu.total_vram(),
            Gpu::Other(gpu) => gpu.total_vram(),
        }
    }

    pub fn temperature(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.temperature(),
            Gpu::Nvidia(gpu) => gpu.temperature(),
            Gpu::Intel(gpu) => gpu.temperature(),
            Gpu::Other(gpu) => gpu.temperature(),
        }
    }

    pub fn power_usage(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.power_usage(),
            Gpu::Nvidia(gpu) => gpu.power_usage(),
            Gpu::Intel(gpu) => gpu.power_usage(),
            Gpu::Other(gpu) => gpu.power_usage(),
        }
    }

    pub fn core_frequency(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.core_frequency(),
            Gpu::Nvidia(gpu) => gpu.core_frequency(),
            Gpu::Intel(gpu) => gpu.core_frequency(),
            Gpu::Other(gpu) => gpu.core_frequency(),
        }
    }

    pub fn vram_frequency(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.vram_frequency(),
            Gpu::Nvidia(gpu) => gpu.vram_frequency(),
            Gpu::Intel(gpu) => gpu.vram_frequency(),
            Gpu::Other(gpu) => gpu.vram_frequency(),
        }
    }

    pub fn power_cap(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.power_cap(),
            Gpu::Nvidia(gpu) => gpu.power_cap(),
            Gpu::Intel(gpu) => gpu.power_cap(),
            Gpu::Other(gpu) => gpu.power_cap(),
        }
    }

    pub fn power_cap_max(&self) -> Result<f64> {
        match self {
            Gpu::Amd(gpu) => gpu.power_cap_max(),
            Gpu::Nvidia(gpu) => gpu.power_cap_max(),
            Gpu::Intel(gpu) => gpu.power_cap_max(),
            Gpu::Other(gpu) => gpu.power_cap_max(),
        }
    }
}
