//! Scales the frame and converts it to NV12 BT.709 with the GPU's video processor. Works on any
//! frame size: the output never changes, so a game switching resolution just gets scaled and
//! the encoder, the replay buffer and any recording carry on.

use std::mem::ManuallyDrop;

use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::gfx::Gfx;

pub struct Converter {
    gfx: Gfx,
    device: ID3D11VideoDevice,
    context: ID3D11VideoContext1,
    enumerator: Option<ID3D11VideoProcessorEnumerator>,
    processor: Option<ID3D11VideoProcessor>,
    output: Option<ID3D11VideoProcessorOutputView>,
    target: ID3D11Texture2D,
    width: u32,
    height: u32,
    source_size: (u32, u32),
    /// Views by texture, since capture alternates between two.
    inputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

fn text(e: windows::core::Error) -> String {
    e.to_string()
}

/// The largest rectangle of the source's shape that fits the output, centered.
fn fit(src: (u32, u32), dst: (u32, u32)) -> RECT {
    let (sw, sh, dw, dh) = (src.0 as u64, src.1 as u64, dst.0 as u64, dst.1 as u64);
    let (w, h) = if sw * dh > dw * sh { (dw, dw * sh / sw) } else { (dh * sw / sh, dh) };
    let (w, h) = (w as i32 & !1, h as i32 & !1);
    let (x, y) = ((dst.0 as i32 - w) / 2 & !1, (dst.1 as i32 - h) / 2 & !1);
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

impl Converter {
    pub fn new(gfx: &Gfx, width: u32, height: u32) -> Result<Converter, String> {
        let device: ID3D11VideoDevice = gfx.device.cast().map_err(|e| format!("no video device: {e}"))?;
        let context: ID3D11VideoContext1 = gfx.context.cast().map_err(|e| format!("no video context: {e}"))?;
        let target = gfx.create_texture(&D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        })?;
        Ok(Converter {
            gfx: gfx.clone(),
            device,
            context,
            enumerator: None,
            processor: None,
            output: None,
            target,
            width,
            height,
            source_size: (0, 0),
            inputs: Vec::new(),
        })
    }

    pub fn target(&self) -> &ID3D11Texture2D {
        &self.target
    }

    fn prepare(&mut self, src: (u32, u32)) -> Result<(), String> {
        if self.processor.is_some() && self.source_size == src {
            return Ok(());
        }
        self.source_size = src;
        self.inputs.clear();
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputWidth: src.0,
            InputHeight: src.1,
            OutputWidth: self.width,
            OutputHeight: self.height,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            ..Default::default()
        };
        unsafe {
            let enumerator = self.device.CreateVideoProcessorEnumerator(&content).map_err(text)?;
            let processor = self.device.CreateVideoProcessor(&enumerator, 0).map_err(text)?;

            let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            let mut view = None;
            self.device.CreateVideoProcessorOutputView(&self.target, &enumerator, &desc, Some(&mut view)).map_err(text)?;
            self.output = view;

            let c = &self.context;
            let dest = fit(src, (self.width, self.height));
            let source = RECT { left: 0, top: 0, right: src.0 as i32, bottom: src.1 as i32 };
            c.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            c.VideoProcessorSetStreamOutputRate(&processor, 0, D3D11_VIDEO_PROCESSOR_OUTPUT_RATE_NORMAL, false, None);
            c.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source));
            c.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest));
            c.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            c.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            c.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            // Black bars for a source of another shape.
            let black = D3D11_VIDEO_COLOR {
                Anonymous: D3D11_VIDEO_COLOR_0 { YCbCr: D3D11_VIDEO_COLOR_YCbCrA { Y: 0.0625, Cb: 0.5, Cr: 0.5, A: 1.0 } },
            };
            c.VideoProcessorSetOutputBackgroundColor(&processor, true, &black);
            self.enumerator = Some(enumerator);
            self.processor = Some(processor);
        }
        Ok(())
    }

    fn input(&mut self, texture: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView, String> {
        let raw = texture.as_raw();
        if let Some((_, view)) = self.inputs.iter().find(|(t, _)| t.as_raw() == raw) {
            return Ok(view.clone());
        }
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut view = None;
        unsafe { self.device.CreateVideoProcessorInputView(texture, self.enumerator.as_ref().unwrap(), &desc, Some(&mut view)) }.map_err(text)?;
        let view = view.unwrap();
        if self.inputs.len() >= 4 {
            self.inputs.remove(0);
        }
        self.inputs.push((texture.clone(), view.clone()));
        Ok(view)
    }

    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<(), String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        self.prepare((desc.Width, desc.Height))?;
        let view = self.input(source)?;
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: ManuallyDrop::new(Some(view)),
            ..Default::default()
        };
        let streams = [stream];
        let result = unsafe {
            self.context.VideoProcessorBlt(
                self.processor.as_ref().unwrap(),
                self.output.as_ref().unwrap(),
                0,
                &streams,
            )
        };
        let [mut stream] = streams;
        unsafe {
            ManuallyDrop::drop(&mut stream.pInputSurface);
            self.gfx.context.Flush();
        }
        result.map_err(|e| format!("video processor: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::fit;

    #[test]
    fn same_shape_fills_the_output() {
        let r = fit((1920, 1080), (1280, 720));
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, 1280, 720));
    }

    #[test]
    fn narrower_source_gets_side_bars() {
        let r = fit((1280, 1024), (2560, 1440));
        assert_eq!((r.top, r.bottom), (0, 1440));
        assert_eq!(r.left, 380);
        assert_eq!(r.right, 2180);
    }
}
