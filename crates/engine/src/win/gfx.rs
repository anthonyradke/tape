//! Finding monitors and creating the D3D11 device everything else shares.

use tape_core::ipc::Monitor;
use windows::core::{s, w, Interface, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, GetMonitorInfoW, DEVMODEW, ENUM_CURRENT_SETTINGS, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::GetCurrentProcess;

const NVIDIA: u32 = 0x10de;
const MONITORINFOF_PRIMARY: u32 = 1;
/// Above most game work, short of REALTIME, which hangs NVENC on some drivers when combined
/// with hardware-accelerated GPU scheduling.
const GPU_PRIORITY_HIGH: i32 = 4;

pub struct OutputInfo {
    pub adapter: IDXGIAdapter1,
    pub output: IDXGIOutput1,
    pub handle: HMONITOR,
    /// `\\.\DISPLAY1`
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub hz: u32,
    pub primary: bool,
    pub gpu: String,
    pub driver: String,
    pub vendor: u32,
}

impl OutputInfo {
    pub fn is_nvidia(&self) -> bool {
        self.vendor == NVIDIA
    }

    pub fn monitor(&self, index: usize) -> Monitor {
        Monitor {
            id: self.id.clone(),
            name: format!("Monitor {}", index + 1),
            width: self.width,
            height: self.height,
            hz: self.hz,
            primary: self.primary,
        }
    }
}

fn wide(text: &[u16]) -> String {
    let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// UMD version as Windows shows it, e.g. 32.0.15.6614. NVIDIA's own number is the last five digits.
fn driver_version(adapter: &IDXGIAdapter1) -> String {
    let Ok(v) = (unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }) else { return String::new() };
    let (a, b, c, d) = ((v >> 48) & 0xffff, (v >> 32) & 0xffff, (v >> 16) & 0xffff, v & 0xffff);
    format!("{a}.{b}.{c}.{d}")
}

pub fn outputs() -> Result<Vec<OutputInfo>, String> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| format!("DXGI unavailable: {e}"))?;
    let mut found = Vec::new();
    for a in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else { break };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
        for o in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else { break };
            let Ok(info) = (unsafe { output.GetDesc() }) else { continue };
            if !info.AttachedToDesktop.as_bool() {
                continue;
            }
            let Ok(output) = output.cast::<IDXGIOutput1>() else { continue };
            let RECT { left, top, right, bottom } = info.DesktopCoordinates;

            let mut monitor = MONITORINFOEXW::default();
            monitor.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            let got = unsafe { GetMonitorInfoW(info.Monitor, &mut monitor as *mut _ as *mut MONITORINFO) };
            let primary = got.as_bool() && monitor.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0;

            let id = wide(&info.DeviceName);
            let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
            let got = unsafe { EnumDisplaySettingsW(PCWSTR(info.DeviceName.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };

            found.push(OutputInfo {
                adapter: adapter.clone(),
                output,
                handle: info.Monitor,
                id,
                width: (right - left) as u32,
                height: (bottom - top) as u32,
                hz: if got.as_bool() { mode.dmDisplayFrequency } else { 0 },
                primary,
                gpu: wide(&desc.Description),
                driver: driver_version(&adapter),
                vendor: desc.VendorId,
            });
        }
    }
    found.sort_by_key(|o| !o.primary);
    if found.is_empty() {
        return Err("No monitors found".into());
    }
    Ok(found)
}

/// The chosen monitor, or the primary one if it's blank or has gone away.
pub fn find(id: &str) -> Result<OutputInfo, String> {
    let mut all = outputs()?;
    let at = all.iter().position(|o| o.id == id).unwrap_or(0);
    Ok(all.swap_remove(at))
}

#[derive(Clone)]
pub struct Gfx {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
}

impl Gfx {
    pub fn new(adapter: &IDXGIAdapter1) -> Result<Gfx, String> {
        let adapter: IDXGIAdapter = adapter.cast().map_err(|e| e.to_string())?;
        let create = |flags: D3D11_CREATE_DEVICE_FLAG| {
            let (mut device, mut context) = (None, None);
            unsafe {
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    flags,
                    Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            }
            .map(|_| (device, context))
        };
        // A GPU with no video engine still captures, which is what the self-test wants to know
        // on one. The converter is what reports that it can't go further.
        let (device, context) = create(D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT)
            .or_else(|_| create(D3D11_CREATE_DEVICE_BGRA_SUPPORT))
            .map_err(|e| format!("couldn't create a D3D11 device: {e}"))?;
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.unwrap(), context.unwrap());
        if let Ok(guard) = context.cast::<ID3D11Multithread>() {
            // The encoder uses this device from inside the driver.
            let _ = unsafe { guard.SetMultithreadProtected(true) };
        }
        raise_gpu_priority(&device);
        Ok(Gfx { device, context })
    }

    pub fn create_texture(&self, desc: &D3D11_TEXTURE2D_DESC) -> Result<ID3D11Texture2D, String> {
        let mut texture = None;
        unsafe { self.device.CreateTexture2D(desc, None, Some(&mut texture)) }.map_err(|e| format!("couldn't create a texture: {e}"))?;
        Ok(texture.unwrap())
    }
}

/// When a game has the GPU flat out, capture and encode work queues behind it and recordings
/// stutter. Failure here only costs smoothness under load.
fn raise_gpu_priority(device: &ID3D11Device) {
    unsafe {
        if let Ok(dxgi) = device.cast::<IDXGIDevice>() {
            let _ = dxgi.SetGPUThreadPriority(7);
        }
        let Ok(gdi) = LoadLibraryW(w!("gdi32.dll")) else { return };
        let Some(raw) = GetProcAddress(gdi, s!("D3DKMTSetProcessSchedulingPriorityClass")) else { return };
        let set: unsafe extern "system" fn(HANDLE, i32) -> i32 = std::mem::transmute(raw);
        let _ = set(GetCurrentProcess(), GPU_PRIORITY_HIGH);
    }
}
