//! Draws the mouse pointer onto a captured frame, since Desktop Duplication leaves it out.

use tape_core::cursor::CursorImage;
use windows::core::{s, PCSTR};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DCOMPILE_OPTIMIZATION_LEVEL3};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use super::gfx::Gfx;
use crate::rotation::Turn;

const SHADER: &str = r#"
cbuffer Quad : register(b0) { float4 rect; uint4 turn; };
struct V { float4 position : SV_Position; float2 uv : TEXCOORD0; };
V vs(uint id : SV_VertexID) {
    V o;
    float2 c = float2(id & 1, id >> 1);
    o.position = float4(lerp(rect.x, rect.z, c.x), lerp(rect.y, rect.w, c.y), 0, 1);
    // The shape is upright on the desktop; on a rotated monitor's image it's turned back.
    // Matches shape_uv in rotation.rs.
    o.uv = turn.x == 1 ? float2(1 - c.y, c.x) : turn.x == 2 ? 1 - c : turn.x == 3 ? float2(c.y, 1 - c.x) : c;
    return o;
}
Texture2D image : register(t0);
SamplerState nearest : register(s0);
float4 ps(V i) : SV_Target { return image.Sample(nearest, i.uv); }
"#;

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob, String> {
    let mut code = None;
    let mut errors = None;
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            None,
            None,
            None,
            entry,
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    result.map_err(|e| format!("shader: {e}"))?;
    code.ok_or_else(|| "shader: no output".to_string())
}

fn bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}

struct Layer {
    view: ID3D11ShaderResourceView,
}

pub struct Overlay {
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    quad: ID3D11Buffer,
    alpha: ID3D11BlendState,
    invert: ID3D11BlendState,
    gfx: Gfx,
    color: Option<Layer>,
    inverted: Option<Layer>,
    size: (u32, u32),
}

fn blend(gfx: &Gfx, source: D3D11_BLEND, dest: D3D11_BLEND) -> Result<ID3D11BlendState, String> {
    let mut desc = D3D11_BLEND_DESC::default();
    desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: source,
        DestBlend: dest,
        BlendOp: D3D11_BLEND_OP_ADD,
        // Leave the frame's own alpha alone.
        SrcBlendAlpha: D3D11_BLEND_ZERO,
        DestBlendAlpha: D3D11_BLEND_ONE,
        BlendOpAlpha: D3D11_BLEND_OP_ADD,
        RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
    };
    let mut state = None;
    unsafe { gfx.device.CreateBlendState(&desc, Some(&mut state)) }.map_err(|e| e.to_string())?;
    Ok(state.unwrap())
}

impl Overlay {
    pub fn new(gfx: &Gfx) -> Result<Overlay, String> {
        let (vs, ps) = (compile(s!("vs"), s!("vs_4_0"))?, compile(s!("ps"), s!("ps_4_0"))?);
        let text = |e: windows::core::Error| e.to_string();
        let (mut vertex, mut pixel, mut sampler, mut quad) = (None, None, None, None);
        unsafe {
            gfx.device.CreateVertexShader(bytes(&vs), None, Some(&mut vertex)).map_err(text)?;
            gfx.device.CreatePixelShader(bytes(&ps), None, Some(&mut pixel)).map_err(text)?;
            let desc = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ..Default::default()
            };
            gfx.device.CreateSamplerState(&desc, Some(&mut sampler)).map_err(text)?;
            let desc = D3D11_BUFFER_DESC {
                ByteWidth: 32,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            gfx.device.CreateBuffer(&desc, None, Some(&mut quad)).map_err(text)?;
        }
        Ok(Overlay {
            vertex: vertex.unwrap(),
            pixel: pixel.unwrap(),
            sampler: sampler.unwrap(),
            quad: quad.unwrap(),
            alpha: blend(gfx, D3D11_BLEND_SRC_ALPHA, D3D11_BLEND_INV_SRC_ALPHA)?,
            // Result = source * (1 - dest) + dest * (1 - source): white flips, black leaves alone.
            invert: blend(gfx, D3D11_BLEND_INV_DEST_COLOR, D3D11_BLEND_INV_SRC_COLOR)?,
            gfx: gfx.clone(),
            color: None,
            inverted: None,
            size: (0, 0),
        })
    }

    pub fn has_shape(&self) -> bool {
        self.color.is_some()
    }

    fn layer(&self, width: u32, height: u32, pixels: &[u8]) -> Result<Layer, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let data = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr().cast(), SysMemPitch: width * 4, SysMemSlicePitch: 0 };
        let (mut texture, mut view) = (None, None);
        unsafe {
            self.gfx.device.CreateTexture2D(&desc, Some(&data), Some(&mut texture)).map_err(|e| e.to_string())?;
            self.gfx.device.CreateShaderResourceView(texture.as_ref().unwrap(), None, Some(&mut view)).map_err(|e| e.to_string())?;
        }
        Ok(Layer { view: view.unwrap() })
    }

    pub fn set_shape(&mut self, image: &CursorImage) {
        if image.width == 0 || image.height == 0 {
            return;
        }
        let Ok(color) = self.layer(image.width, image.height, &image.color) else { return };
        self.inverted = image.invert.as_ref().and_then(|p| self.layer(image.width, image.height, p).ok());
        self.color = Some(color);
        self.size = (image.width, image.height);
    }

    /// `at` is where the shape's top left goes, in desktop pixels. Duplication reports it that way
    /// already, so the hotspot isn't subtracted again. `turn` is how the desktop is turned from
    /// `frame`, the image being drawn on.
    pub fn draw(&self, gfx: &Gfx, target: &ID3D11Texture2D, at: POINT, frame: (u32, u32), turn: Turn) -> Result<(), String> {
        let (Some(color), width, height) = (&self.color, frame.0 as f32, frame.1 as f32) else { return Ok(()) };
        let ndc_x = |x: i32| x as f32 / width * 2.0 - 1.0;
        let ndc_y = |y: i32| 1.0 - y as f32 / height * 2.0;
        let (left, top, right, bottom) = turn.desktop_rect_in_image((at.x, at.y), self.size, frame);
        let rect = [ndc_x(left), ndc_y(top), ndc_x(right), ndc_y(bottom)];
        // The constant buffer: the rectangle, then the quarter turns padded to 16 bytes.
        let mut constants = [0u32; 8];
        for (slot, value) in constants.iter_mut().zip(rect) {
            *slot = value.to_bits();
        }
        constants[4] = turn.quarters();
        let ctx = &gfx.context;
        let mut rtv = None;
        unsafe {
            gfx.device.CreateRenderTargetView(target, None, Some(&mut rtv)).map_err(|e| e.to_string())?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.quad, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped)).map_err(|e| e.to_string())?;
            std::ptr::copy_nonoverlapping(constants.as_ptr(), mapped.pData.cast::<u32>(), constants.len());
            ctx.Unmap(&self.quad, 0);

            ctx.OMSetRenderTargets(Some(&[rtv.clone()]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT { Width: width, Height: height, MaxDepth: 1.0, ..Default::default() }]));
            ctx.RSSetScissorRects(Some(&[RECT { left: 0, top: 0, right: frame.0 as i32, bottom: frame.1 as i32 }]));
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            ctx.IASetInputLayout(None);
            ctx.VSSetShader(&self.vertex, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.quad.clone())]));
            ctx.PSSetShader(&self.pixel, None);
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));

            ctx.OMSetBlendState(&self.alpha, None, u32::MAX);
            ctx.PSSetShaderResources(0, Some(&[Some(color.view.clone())]));
            ctx.Draw(4, 0);
            if let Some(inverted) = &self.inverted {
                ctx.OMSetBlendState(&self.invert, None, u32::MAX);
                ctx.PSSetShaderResources(0, Some(&[Some(inverted.view.clone())]));
                ctx.Draw(4, 0);
            }
            ctx.PSSetShaderResources(0, Some(&[None]));
            ctx.OMSetRenderTargets(Some(&[None]), None);
        }
        Ok(())
    }
}
