//! Getting the desktop image into a texture, by either of the two APIs Windows offers.
//! Desktop Duplication is the default: it has the least overhead on a single-GPU desktop and
//! copes with fullscreen games. Windows Graphics Capture is the fallback for setups where
//! duplication is refused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tape_core::cursor;
use windows::core::{IInspectable, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{E_ACCESSDENIED, POINT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270, DXGI_MODE_ROTATION_ROTATE90,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use super::cursor::Overlay;
use super::gfx::{Gfx, OutputInfo};
use crate::log;
use crate::rotation::Turn;

pub enum CaptureError {
    /// Normal while a UAC prompt or the lock screen is up, the monitor is off or the session is
    /// disconnected. Reopen and keep going; the host doesn't sound the alarm for these.
    Transient(String),
    /// This API isn't going to work here. Worth trying the other one.
    Refused(String),
}

impl CaptureError {
    pub fn message(&self) -> &str {
        match self {
            CaptureError::Transient(m) | CaptureError::Refused(m) => m,
        }
    }
}

pub trait Capture {
    /// Waits up to `timeout_ms` for a new image. True if there is one.
    fn next(&mut self, timeout_ms: u32) -> Result<bool, CaptureError>;
    /// The current image, ready for conversion.
    fn frame(&mut self) -> Option<ID3D11Texture2D>;
    fn method(&self) -> &'static str;
    /// How the desktop is turned from the image `frame` gives, which the converter undoes.
    fn turn(&self) -> Turn {
        Turn::None
    }
}

/// The latest desktop image, in a texture we own so the source can be released right away.
#[derive(Default)]
struct Store {
    texture: Option<ID3D11Texture2D>,
    desc: D3D11_TEXTURE2D_DESC,
}

impl Store {
    fn copy(&mut self, gfx: &Gfx, source: &ID3D11Texture2D) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        let same = self.texture.is_some()
            && desc.Width == self.desc.Width
            && desc.Height == self.desc.Height
            && desc.Format == self.desc.Format;
        if !same {
            self.desc = desc;
            self.texture = Some(gfx.create_texture(&target_desc(&desc))?);
        }
        unsafe { gfx.context.CopyResource(self.texture.as_ref().unwrap(), source) };
        Ok(())
    }
}

fn target_desc(from: &D3D11_TEXTURE2D_DESC) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: from.Width,
        Height: from.Height,
        MipLevels: 1,
        ArraySize: 1,
        Format: from.Format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    }
}

pub struct Duplication {
    gfx: Gfx,
    duplication: IDXGIOutputDuplication,
    store: Store,
    composed: Store,
    overlay: Option<Overlay>,
    pointer_at: POINT,
    pointer_visible: bool,
    shape: Vec<u8>,
    turn: Turn,
}

impl Duplication {
    pub fn open(gfx: &Gfx, output: &OutputInfo, show_cursor: bool) -> Result<Duplication, CaptureError> {
        // DuplicateOutput1 also covers HDR desktops, which plain DuplicateOutput refuses, and hands
        // them over converted to 8-bit. Windows before 1803 only has the plain one.
        let newer = output
            .output
            .cast::<IDXGIOutput5>()
            .and_then(|o| unsafe { o.DuplicateOutput1(&gfx.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]) });
        let duplication = match newer {
            Ok(d) => Ok(d),
            Err(e) if e.code() == E_ACCESSDENIED => Err(e),
            Err(_) => unsafe { output.output.DuplicateOutput(&gfx.device) },
        };
        let duplication = duplication.map_err(|e| {
            if e.code() == E_ACCESSDENIED {
                CaptureError::Transient("Capture is paused by a Windows security prompt or the lock screen".into())
            } else if e.code() == DXGI_ERROR_SESSION_DISCONNECTED {
                CaptureError::Transient("Capture is paused while the session is disconnected".into())
            } else {
                CaptureError::Refused(format!("Desktop Duplication: {e}"))
            }
        })?;
        // A monitor turned to portrait is captured as its panel scans out, landscape, so the
        // converter turns the image and the pointer is placed the same way.
        let rotation = unsafe { duplication.GetDesc() }.Rotation;
        let turn = match rotation {
            DXGI_MODE_ROTATION_ROTATE90 => Turn::Cw90,
            DXGI_MODE_ROTATION_ROTATE180 => Turn::Cw180,
            DXGI_MODE_ROTATION_ROTATE270 => Turn::Cw270,
            _ => Turn::None,
        };
        log!("capture: {} {}x{}, rotated {} degrees", output.id, output.width, output.height, turn.degrees());
        let overlay = if show_cursor {
            // Capture is worth having without a pointer, so this doesn't stop anything.
            Overlay::new(gfx).map_err(|e| log!("pointer overlay: {e}")).ok()
        } else {
            None
        };
        Ok(Duplication {
            gfx: gfx.clone(),
            duplication,
            store: Store::default(),
            composed: Store::default(),
            overlay,
            pointer_at: POINT::default(),
            pointer_visible: false,
            shape: Vec::new(),
            turn,
        })
    }

    fn read_pointer(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            self.pointer_visible = info.PointerPosition.Visible.as_bool();
            self.pointer_at = info.PointerPosition.Position;
        }
        if info.PointerShapeBufferSize == 0 {
            return;
        }
        let Some(overlay) = &mut self.overlay else { return };
        self.shape.resize(info.PointerShapeBufferSize as usize, 0);
        let mut used = 0;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let got = unsafe {
            self.duplication.GetFramePointerShape(
                self.shape.len() as u32,
                self.shape.as_mut_ptr().cast(),
                &mut used,
                &mut shape,
            )
        };
        if got.is_err() {
            return;
        }
        let (w, h, pitch, data) = (shape.Width, shape.Height, shape.Pitch, &self.shape[..used as usize]);
        let image = match DXGI_OUTDUPL_POINTER_SHAPE_TYPE(shape.Type as i32) {
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME => cursor::monochrome(w, h, pitch, data),
            DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR => cursor::masked(w, h, pitch, data),
            _ => cursor::color(w, h, pitch, data),
        };
        if let Some(image) = image {
            overlay.set_shape(&image);
        }
    }
}

impl Capture for Duplication {
    fn next(&mut self, timeout_ms: u32) -> Result<bool, CaptureError> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match unsafe { self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) => return Err(CaptureError::Transient(format!("Desktop Duplication lost: {e}"))),
        }
        self.read_pointer(&info);
        let new_image = info.LastPresentTime != 0 || self.store.texture.is_none();
        let copied = match (&resource, new_image) {
            (Some(r), true) => r
                .cast::<ID3D11Texture2D>()
                .map_err(|e| e.to_string())
                .and_then(|texture| self.store.copy(&self.gfx, &texture)),
            _ => Ok(()),
        };
        // Hold the frame as briefly as possible: while it's held, the desktop can't update.
        drop(resource);
        let _ = unsafe { self.duplication.ReleaseFrame() };
        copied.map_err(CaptureError::Transient)?;
        Ok(new_image || info.LastMouseUpdateTime != 0)
    }

    fn frame(&mut self) -> Option<ID3D11Texture2D> {
        let source = self.store.texture.clone()?;
        let Some(overlay) = self.overlay.as_mut().filter(|o| o.has_shape() && self.pointer_visible) else {
            return Some(source);
        };
        if self.composed.copy(&self.gfx, &source).is_err() {
            return Some(source);
        }
        let target = self.composed.texture.clone()?;
        let size = (self.store.desc.Width, self.store.desc.Height);
        match overlay.draw(&self.gfx, &target, self.pointer_at, size, self.turn) {
            Ok(()) => Some(target),
            Err(_) => Some(source),
        }
    }

    fn method(&self) -> &'static str {
        "Desktop Duplication"
    }

    fn turn(&self) -> Turn {
        self.turn
    }
}

pub struct GraphicsCapture {
    gfx: Gfx,
    device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
    closed: Arc<AtomicBool>,
    store: Store,
}

impl GraphicsCapture {
    pub fn open(gfx: &Gfx, output: &OutputInfo, show_cursor: bool) -> Result<GraphicsCapture, CaptureError> {
        Self::try_open(gfx, output, show_cursor).map_err(|e| CaptureError::Refused(format!("Windows Graphics Capture: {e}")))
    }

    fn try_open(gfx: &Gfx, output: &OutputInfo, show_cursor: bool) -> windows::core::Result<GraphicsCapture> {
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL));
        }
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(output.handle)? };
        let dxgi: IDXGIDevice = gfx.device.cast()?;
        let inspectable: IInspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
        let device: IDirect3DDevice = inspectable.cast()?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)?;
        let session = pool.CreateCaptureSession(&item)?;
        session.SetIsCursorCaptureEnabled(show_cursor)?;
        // Windows 11 only. On older builds the yellow capture border stays.
        let _ = session.SetIsBorderRequired(false);

        let closed = Arc::new(AtomicBool::new(false));
        let flag = closed.clone();
        item.Closed(&TypedEventHandler::new(move |_, _| {
            flag.store(true, Ordering::Relaxed);
            Ok(())
        }))?;
        session.StartCapture()?;
        Ok(GraphicsCapture { gfx: gfx.clone(), device, pool, session, size, closed, store: Store::default() })
    }

    fn take(&mut self) -> Result<bool, String> {
        // A free-threaded pool returns null rather than an error when no frame is waiting.
        let Ok(frame) = self.pool.TryGetNextFrame() else { return Ok(false) };
        let text = |e: windows::core::Error| e.to_string();
        let surface = frame.Surface().map_err(text)?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(text)?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(text)?;
        self.store.copy(&self.gfx, &texture)?;
        let size = frame.ContentSize().map_err(text)?;
        if size.Width != self.size.Width || size.Height != self.size.Height {
            self.size = size;
            self.pool
                .Recreate(&self.device, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size)
                .map_err(text)?;
        }
        Ok(true)
    }
}

impl Drop for GraphicsCapture {
    /// Close stops capture and frees the pool's buffers now, rather than whenever the last
    /// reference goes, which WinRT's side can hold on to, keeping the capture border up.
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

impl Capture for GraphicsCapture {
    fn next(&mut self, timeout_ms: u32) -> Result<bool, CaptureError> {
        let until = Instant::now() + Duration::from_millis(timeout_ms as u64);
        loop {
            if self.closed.load(Ordering::Relaxed) {
                return Err(CaptureError::Transient("Windows Graphics Capture: monitor went away".into()));
            }
            if self.take().map_err(CaptureError::Transient)? {
                return Ok(true);
            }
            if Instant::now() >= until {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn frame(&mut self) -> Option<ID3D11Texture2D> {
        self.store.texture.clone()
    }

    fn method(&self) -> &'static str {
        "Windows Graphics Capture"
    }
}
