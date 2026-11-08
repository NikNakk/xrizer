//! OpenGL submissions on Windows: the session stays on D3D11, and the D3D11 backend
//! copies each eye from a D3D11 texture this module provides:
//!
//! - GPU: when Mesa can export the GL texture's Metal texture
//!   (`wglExportMetalTextureMWXR`, Zink over a Metal Vulkan driver) and the D3D11
//!   device is DXMT (`IDXMTNativeDevice2`), the same Metal texture is imported as
//!   D3D11 (typeless, so it can be read in the swapchain's format) and copied with
//!   one GPU operation per eye. `glFinish` orders GL's rendering before it.
//! - CPU otherwise: the region is read back and uploaded to a staging texture.
//!
//! GL rows run bottom to top. The GPU path copies each eye with one draw that reads
//! rows in reverse (a Metal render pass; a blit cannot flip), or a plain blit when
//! the application's bounds already invert it; the CPU path reverses rows itself.
//!
//! Every GL call runs on the submitting thread, with the application's context current.

use std::ffi::{c_void, CStr};
use std::sync::Mutex;

use openvr as vr;
use openxr as xr;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BOX, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::core::{GUID, HRESULT, Interface};
use windows::Win32::Graphics::Direct3D::{D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_SRV_DIMENSION_TEXTURE2D};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_RENDER_TARGET_VIEW_DESC,
    D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
    D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0, D3D11_TEX2D_ARRAY_RTV,
    D3D11_TEX2D_SRV, D3D11_USAGE_STAGING, D3D11_VIEWPORT, ID3D11Buffer, ID3D11PixelShader,
    ID3D11RenderTargetView, ID3D11ShaderResourceView, ID3D11VertexShader,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};

/// DXGI_FORMAT_R8G8B8A8_UNORM: GL textures are always read back as 8-bit RGBA.
pub const STAGING_FORMAT: u32 = 28;

type GLenum = u32;
type GLint = i32;
type GLuint = u32;
type GLsizei = i32;

const GL_TEXTURE_2D: GLenum = 0x0DE1;
const GL_TEXTURE_BINDING_2D: GLenum = 0x8069;
const GL_TEXTURE_WIDTH: GLenum = 0x1000;
const GL_TEXTURE_HEIGHT: GLenum = 0x1001;
const GL_TEXTURE_INTERNAL_FORMAT: GLenum = 0x1003;
const GL_READ_FRAMEBUFFER: GLenum = 0x8CA8;
const GL_READ_FRAMEBUFFER_BINDING: GLenum = 0x8CAA;
const GL_COLOR_ATTACHMENT0: GLenum = 0x8CE0;
const GL_FRAMEBUFFER_COMPLETE: GLenum = 0x8CD5;
const GL_READ_BUFFER: GLenum = 0x0C02;
const GL_PIXEL_PACK_BUFFER: GLenum = 0x88EB;
const GL_PIXEL_PACK_BUFFER_BINDING: GLenum = 0x88ED;
const GL_PACK_ALIGNMENT: GLenum = 0x0D05;
const GL_PACK_ROW_LENGTH: GLenum = 0x0D02;
const GL_PACK_SKIP_ROWS: GLenum = 0x0D03;
const GL_PACK_SKIP_PIXELS: GLenum = 0x0D04;
const GL_RGBA: GLenum = 0x1908;
const GL_UNSIGNED_BYTE: GLenum = 0x1401;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

/// Mesa's `struct wgl_metal_texture_mwxr` (include/GL/mesa_glinterop.h).
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct MetalTexture {
    mtl_texture: u64,
    dxgi_format: u32,
    width: u32,
    height: u32,
    levels: u32,
    samples: u32,
}

/// The typeless family of a DXGI format, so one texture can be read as UNORM or sRGB.
fn typeless(format: u32) -> Option<u32> {
    match format {
        28 | 29 => Some(27), // R8G8B8A8
        87 | 91 => Some(90), // B8G8R8A8
        24 => Some(23),      // R10G10B10A2
        10 => Some(9),       // R16G16B16A16
        _ => None,
    }
}

/// DXMT's IDXMTNativeDevice2 (include/dxmt_native_interop.h).
const IID_DXMT_NATIVE_DEVICE2: GUID = GUID::from_u128(0x5a6d2e1b_b10a_4ab7_8ade_d9a64c417283);

#[repr(C)]
struct NativeDevice2Vtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_metal_device: usize,
    import_metal_texture: unsafe extern "system" fn(
        *mut c_void,
        u64,
        *const D3D11_TEXTURE2D_DESC,
        *mut *mut c_void,
    ) -> HRESULT,
    import_metal_shared_event: usize,
}

struct NativeDevice(*mut c_void);
// The interface belongs to the D3D11 device, which the backend already shares.
unsafe impl Send for NativeDevice {}

impl NativeDevice {
    fn query(device: &ID3D11Device) -> Option<Self> {
        let mut ptr = std::ptr::null_mut();
        let hr = unsafe { device.query(&IID_DXMT_NATIVE_DEVICE2, &mut ptr) };
        (hr.is_ok() && !ptr.is_null()).then_some(Self(ptr))
    }

    fn import(&self, metal: &MetalTexture) -> Result<ID3D11Texture2D, HRESULT> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: metal.width,
            Height: metal.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT(typeless(metal.dxgi_format).unwrap_or(0) as i32),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let mut out = std::ptr::null_mut();
        let hr = unsafe {
            let vtbl = &**(self.0 as *mut *const NativeDevice2Vtbl);
            (vtbl.import_metal_texture)(self.0, metal.mtl_texture, &desc, &mut out)
        };
        if hr.is_err() || out.is_null() {
            return Err(hr);
        }
        Ok(unsafe { ID3D11Texture2D::from_raw(out) })
    }
}

impl Drop for NativeDevice {
    fn drop(&mut self) {
        unsafe {
            let vtbl = &**(self.0 as *mut *const NativeDevice2Vtbl);
            (vtbl.release)(self.0);
        }
    }
}

/// A GL texture's Metal texture, imported into D3D11.
struct Import {
    gl: GLuint,
    mtl_texture: u64,
    /// The texture's own DXGI format; the import is its typeless form.
    format: u32,
    texture: ID3D11Texture2D,
}

#[derive(Default)]
struct Gpu {
    native: Option<NativeDevice>,
    flip: Option<FlipCopy>,
    /// Set once the GPU path is known not to work; the CPU path is used from then on.
    disabled: bool,
    imports: Vec<Import>,
}

struct GlFns {
    finish: unsafe extern "system" fn(),
    export_metal: Option<unsafe extern "C" fn(GLuint, *mut MetalTexture) -> i32>,
    get_integerv: unsafe extern "system" fn(GLenum, *mut GLint),
    bind_texture: unsafe extern "system" fn(GLenum, GLuint),
    get_tex_level_parameteriv: unsafe extern "system" fn(GLenum, GLint, GLenum, *mut GLint),
    read_pixels:
        unsafe extern "system" fn(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, *mut c_void),
    read_buffer: unsafe extern "system" fn(GLenum),
    pixel_storei: unsafe extern "system" fn(GLenum, GLint),
    get_error: unsafe extern "system" fn() -> GLenum,
    gen_framebuffers: unsafe extern "system" fn(GLsizei, *mut GLuint),
    bind_framebuffer: unsafe extern "system" fn(GLenum, GLuint),
    framebuffer_texture_2d: unsafe extern "system" fn(GLenum, GLenum, GLenum, GLuint, GLint),
    check_framebuffer_status: unsafe extern "system" fn(GLenum) -> GLenum,
    bind_buffer: unsafe extern "system" fn(GLenum, GLuint),
}

impl GlFns {
    /// Needs the application's GL context current, for wglGetProcAddress.
    fn load() -> Option<Self> {
        unsafe {
            let module = GetModuleHandleA(c"opengl32.dll".as_ptr().cast());
            if module.is_null() {
                log::error!("OpenGL submission, but opengl32.dll is not loaded");
                return None;
            }
            let wgl_get_proc_address: unsafe extern "system" fn(*const u8) -> *mut c_void =
                std::mem::transmute(non_null(GetProcAddress(
                    module,
                    c"wglGetProcAddress".as_ptr().cast(),
                ))?);
            let core = |name: &CStr| non_null(GetProcAddress(module, name.as_ptr().cast()));
            let ext = |name: &CStr| {
                non_null(wgl_get_proc_address(name.as_ptr().cast())).or_else(|| {
                    log::error!("OpenGL function {name:?} is unavailable");
                    None
                })
            };
            macro_rules! load {
                ($how:ident, $name:literal) => {
                    std::mem::transmute($how($name)?)
                };
            }
            let export_metal = non_null(wgl_get_proc_address(
                c"wglExportMetalTextureMWXR".as_ptr().cast(),
            ))
            .map(|p| std::mem::transmute(p));
            Some(Self {
                finish: load!(core, c"glFinish"),
                export_metal,
                get_integerv: load!(core, c"glGetIntegerv"),
                bind_texture: load!(core, c"glBindTexture"),
                get_tex_level_parameteriv: load!(core, c"glGetTexLevelParameteriv"),
                read_pixels: load!(core, c"glReadPixels"),
                read_buffer: load!(core, c"glReadBuffer"),
                pixel_storei: load!(core, c"glPixelStorei"),
                get_error: load!(core, c"glGetError"),
                gen_framebuffers: load!(ext, c"glGenFramebuffers"),
                bind_framebuffer: load!(ext, c"glBindFramebuffer"),
                framebuffer_texture_2d: load!(ext, c"glFramebufferTexture2D"),
                check_framebuffer_status: load!(ext, c"glCheckFramebufferStatus"),
                bind_buffer: load!(ext, c"glBindBuffer"),
            })
        }
    }

    fn export_metal(&self, texture: GLuint) -> Option<MetalTexture> {
        let export = self.export_metal?;
        let mut metal = MetalTexture::default();
        (unsafe { export(texture, &mut metal) } != 0 && metal.mtl_texture != 0).then_some(metal)
    }

    fn integer(&self, name: GLenum) -> GLint {
        let mut value = 0;
        unsafe { (self.get_integerv)(name, &mut value) };
        value
    }

    /// Width, height and internal format of level 0.
    fn texture_info(&self, texture: GLuint) -> (i32, i32, i32) {
        let [mut width, mut height, mut format] = [0; 3];
        unsafe {
            let previous = self.integer(GL_TEXTURE_BINDING_2D);
            (self.bind_texture)(GL_TEXTURE_2D, texture);
            (self.get_tex_level_parameteriv)(GL_TEXTURE_2D, 0, GL_TEXTURE_WIDTH, &mut width);
            (self.get_tex_level_parameteriv)(GL_TEXTURE_2D, 0, GL_TEXTURE_HEIGHT, &mut height);
            (self.get_tex_level_parameteriv)(
                GL_TEXTURE_2D,
                0,
                GL_TEXTURE_INTERNAL_FORMAT,
                &mut format,
            );
            (self.bind_texture)(GL_TEXTURE_2D, previous as GLuint);
        }
        (width, height, format)
    }
}

fn non_null(p: *mut c_void) -> Option<*mut c_void> {
    (!p.is_null()).then_some(p)
}

struct Staging {
    texture: ID3D11Texture2D,
    width: u32,
    height: u32,
}

#[derive(Default)]
struct State {
    gpu: Gpu,
    fbo: GLuint,
    pixels: Vec<u8>,
    staging: [Option<Staging>; 2],
    failures: u32,
    app_errors: u32,
    described: Option<(GLuint, i32, i32, i32)>,
    copies: u32,
    /// Successful copies per eye, for XRIZER_GL_CAPTURE.
    eye_copies: [u32; 2],
    copy_time: std::time::Duration,
    /// Start of the current reporting window, for the frame rate.
    window_start: Option<std::time::Instant>,
    /// Of copy_time, the time spent waiting for the application's GL work (glFinish).
    gl_wait: std::time::Duration,
    copy_bytes: usize,
}

pub struct GlSource {
    gl: GlFns,
    state: Mutex<State>,
    /// XRIZER_GL_CAPTURE: a folder for PNGs of what is sent to the runtime.
    capture_dir: Option<std::path::PathBuf>,
}

/// Every this many copies of an eye, up to CAPTURES_PER_EYE times.
const CAPTURE_INTERVAL: u32 = 600;
const CAPTURES_PER_EYE: u32 = 5;

/// Saves RGBA rows as a PNG as presented: rows in GL order (`bottom_up`) are flipped,
/// as the projection layer flips them.
fn capture(
    dir: std::path::PathBuf,
    name: String,
    mut pixels: Vec<u8>,
    width: u32,
    height: u32,
    bottom_up: bool,
) {
    std::thread::spawn(move || {
        if bottom_up {
            flip_rows(&mut pixels, width as usize * 4, height as usize);
        }
        // Applications often leave alpha at zero; the runtime ignores it.
        for alpha in pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        let path = dir.join(name);
        match image::save_buffer(&path, &pixels, width, height, image::ColorType::Rgba8) {
            Ok(()) => log::info!("captured {}", path.display()),
            Err(error) => log::error!("capturing {} failed: {error}", path.display()),
        }
    });
}

const FLIP_HLSL: &str = r#"
cbuffer Region : register(b0)
{
    int4 region; // x, y (GL row of the region's bottom), width, height
};
Texture2D<float4> source : register(t0);

float4 vs(uint id : SV_VertexID) : SV_Position
{
    float2 uv = float2((id << 1) & 2, id & 2);
    return float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
}

float4 ps(float4 position : SV_Position) : SV_Target
{
    int2 p = int2(position.xy);
    // The top output row is the region's top GL row.
    return source.Load(int3(region.x + p.x, region.y + region.w - 1 - p.y, 0));
}
"#;

/// Copies a texture region upside down with one draw: GL rows run bottom to top.
/// The source is read in the destination's format (it is imported typeless), so
/// sRGB values decode and re-encode unchanged.
struct FlipCopy {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    constants: ID3D11Buffer,
    /// (source, format) and (destination, slice) views, kept as long as the objects.
    sources: Vec<(ID3D11Texture2D, i32, ID3D11ShaderResourceView)>,
    targets: Vec<(ID3D11Texture2D, u32, ID3D11RenderTargetView)>,
}
// Owned by the D3D11 backend's device, like the rest of its state.
unsafe impl Send for FlipCopy {}

impl FlipCopy {
    fn new(device: &ID3D11Device) -> Result<Self, String> {
        use super::d3d11_stage::{blob_bytes, compile_shader};
        let vs_code = compile_shader(FLIP_HLSL, "vs", "vs_5_0")?;
        let ps_code = compile_shader(FLIP_HLSL, "ps", "ps_5_0")?;
        let (mut vs, mut ps, mut constants) = (None, None, None);
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        unsafe {
            device
                .CreateVertexShader(blob_bytes(&vs_code), None, Some(&mut vs))
                .map_err(|e| format!("CreateVertexShader: {e}"))?;
            device
                .CreatePixelShader(blob_bytes(&ps_code), None, Some(&mut ps))
                .map_err(|e| format!("CreatePixelShader: {e}"))?;
            device
                .CreateBuffer(&desc, None, Some(&mut constants))
                .map_err(|e| format!("CreateBuffer: {e}"))?;
        }
        Ok(Self {
            vs: vs.ok_or("no vertex shader")?,
            ps: ps.ok_or("no pixel shader")?,
            constants: constants.ok_or("no constant buffer")?,
            sources: Vec::new(),
            targets: Vec::new(),
        })
    }

    fn draw(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        src_box: &D3D11_BOX,
        dst: &ID3D11Texture2D,
        dst_slice: u32,
    ) -> Result<(), String> {
        let mut dst_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { dst.GetDesc(&mut dst_desc) };
        let format = dst_desc.Format;

        let srv = match self
            .sources
            .iter()
            .find(|(t, f, _)| t == src && *f == format.0)
        {
            Some((_, _, view)) => view.clone(),
            None => {
                let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_SRV {
                            MostDetailedMip: 0,
                            MipLevels: 1,
                        },
                    },
                };
                let mut view = None;
                unsafe { device.CreateShaderResourceView(src, Some(&desc), Some(&mut view)) }
                    .map_err(|e| format!("CreateShaderResourceView(format {}): {e}", format.0))?;
                let view = view.ok_or("no shader resource view")?;
                // Sources replaced by the application are released with their views.
                self.sources.retain(|(t, _, _)| t != src);
                self.sources.push((src.clone(), format.0, view.clone()));
                view
            }
        };
        let rtv = match self
            .targets
            .iter()
            .find(|(t, s, _)| t == dst && *s == dst_slice)
        {
            Some((_, _, view)) => view.clone(),
            None => {
                let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                            MipSlice: 0,
                            FirstArraySlice: dst_slice,
                            ArraySize: 1,
                        },
                    },
                };
                let mut view = None;
                unsafe { device.CreateRenderTargetView(dst, Some(&desc), Some(&mut view)) }
                    .map_err(|e| format!("CreateRenderTargetView(slice {dst_slice}): {e}"))?;
                let view = view.ok_or("no render target view")?;
                // Swapchain images come in small sets; keep a bounded cache.
                if self.targets.len() >= 16 {
                    self.targets.remove(0);
                }
                self.targets.push((dst.clone(), dst_slice, view.clone()));
                view
            }
        };

        let (width, height) = (src_box.right - src_box.left, src_box.bottom - src_box.top);
        let region = [src_box.left as i32, src_box.top as i32, width as i32, height as i32];
        unsafe {
            context.UpdateSubresource(&self.constants, 0, None, region.as_ptr().cast(), 0, 0);
            context.IASetInputLayout(None);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vs, None);
            context.PSSetShader(&self.ps, None);
            context.PSSetShaderResources(0, Some(&[Some(srv)]));
            context.PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: width as f32,
                Height: height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            context.OMSetRenderTargets(Some(&[Some(rtv)]), None);
            context.Draw(3, 0);
            // This device is the backend's own; leave nothing bound for its copies.
            context.ClearState();
        }
        Ok(())
    }
}

/// Copies a box of an RGBA8 texture to the CPU, for captures only: it waits for the GPU.
fn read_back(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    src_box: &D3D11_BOX,
) -> Option<Vec<u8>> {
    let (width, height) = (src_box.right - src_box.left, src_box.bottom - src_box.top);
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    unsafe {
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging)).ok()?;
        let staging = staging?;
        context.CopySubresourceRegion(&staging, 0, 0, 0, 0, texture, 0, Some(src_box));
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
        let row_bytes = width as usize * 4;
        let mut pixels = vec![0u8; row_bytes * height as usize];
        for row in 0..height as usize {
            let src = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
            std::ptr::copy_nonoverlapping(src, pixels[row * row_bytes..].as_mut_ptr(), row_bytes);
        }
        context.Unmap(&staging, 0);
        Some(pixels)
    }
}

/// A region of a GL texture in GL coordinates (origin bottom left).
pub struct GlRegion {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn region_from_bounds(width: i32, height: i32, bounds: vr::VRTextureBounds_t) -> GlRegion {
    let x0 = (bounds.uMin * width as f32).round() as i32;
    let x1 = (bounds.uMax * width as f32).round() as i32;
    let y0 = (bounds.vMin * height as f32).round() as i32;
    let y1 = (bounds.vMax * height as f32).round() as i32;
    let left = x0.min(x1).clamp(0, width);
    let right = x0.max(x1).clamp(0, width);
    let bottom = y0.min(y1).clamp(0, height);
    let top = y0.max(y1).clamp(0, height);
    GlRegion {
        x: left,
        y: bottom,
        width: right - left,
        height: top - bottom,
    }
}

/// Reverses the order of `height` rows of `row_bytes` each.
pub fn flip_rows(pixels: &mut [u8], row_bytes: usize, height: usize) {
    for row in 0..height / 2 {
        let (upper, lower) = pixels.split_at_mut((height - 1 - row) * row_bytes);
        upper[row * row_bytes..(row + 1) * row_bytes].swap_with_slice(&mut lower[..row_bytes]);
    }
}

impl GlSource {
    pub fn new() -> Option<Self> {
        let gl = GlFns::load()?;
        let mut fbo = 0;
        unsafe { (gl.gen_framebuffers)(1, &mut fbo) };
        log::info!(
            "OpenGL submissions go through D3D11; Mesa Metal export {}",
            if gl.export_metal.is_some() { "available" } else { "unavailable: CPU copies" }
        );
        let capture_dir = std::env::var_os("XRIZER_GL_CAPTURE").map(std::path::PathBuf::from);
        if let Some(dir) = &capture_dir {
            if let Err(error) = std::fs::create_dir_all(dir) {
                log::error!("XRIZER_GL_CAPTURE {}: {error}", dir.display());
            }
        }
        // XRIZER_GL_CPU=1 forces CPU copies, for comparisons.
        let force_cpu = std::env::var("XRIZER_GL_CPU").is_ok_and(|v| v == "1");
        Some(Self {
            gl,
            capture_dir,
            state: Mutex::new(State {
                gpu: Gpu {
                    disabled: force_cpu,
                    ..Default::default()
                },
                fbo,
                ..Default::default()
            }),
        })
    }

    /// The swapchain extent for this texture and bounds.
    pub fn extent(&self, texture: GLuint, bounds: vr::VRTextureBounds_t) -> xr::Extent2Di {
        let (width, height, format) = self.gl.texture_info(texture);
        let region = region_from_bounds(width, height, bounds);
        let mut state = self.state.lock().unwrap();
        if state.described != Some((texture, width, height, format)) {
            state.described = Some((texture, width, height, format));
            log::info!(
                "OpenGL texture {texture}: {width}x{height}, internal format {format:#x}, region {}x{}",
                region.width,
                region.height
            );
        }
        xr::Extent2Di {
            width: region.width.max(1),
            height: region.height.max(1),
        }
    }

    /// The DXGI format of the swapchain base for this texture: the Metal texture's own
    /// when it can be blitted, otherwise the staging format.
    pub fn swapchain_format(&self, device: &ID3D11Device, texture: GLuint) -> u32 {
        let mut state = self.state.lock().unwrap();
        match self.gpu_import(&mut state, device, texture) {
            Some(i) => state.gpu.imports[i].format,
            None => STAGING_FORMAT,
        }
    }

    /// The imported D3D11 texture for a GL texture's current Metal texture, as an index
    /// into `state.gpu.imports`, or None when the GPU path is unavailable.
    fn gpu_import(&self, state: &mut State, device: &ID3D11Device, texture: GLuint) -> Option<usize> {
        if state.gpu.disabled {
            return None;
        }
        let disable = |state: &mut State, why: String| {
            log::warn!("OpenGL texture {texture}: no Metal blit ({why}); using CPU copies");
            state.gpu.disabled = true;
            None
        };
        let Some(metal) = self.gl.export_metal(texture) else {
            return disable(state, "Mesa cannot export its Metal texture".into());
        };
        if let Some(i) = state
            .gpu
            .imports
            .iter()
            .position(|i| i.gl == texture && i.mtl_texture == metal.mtl_texture)
        {
            return Some(i);
        }
        if typeless(metal.dxgi_format).is_none() || metal.levels != 1 || metal.samples != 1 {
            return disable(state, format!("unsupported texture {metal:?}"));
        }
        if state.gpu.native.is_none() {
            state.gpu.native = NativeDevice::query(device);
        }
        let Some(native) = &state.gpu.native else {
            return disable(state, "the D3D11 device is not DXMT".into());
        };
        match native.import(&metal) {
            Ok(imported) => {
                log::info!(
                    "OpenGL texture {texture}: imported Metal texture {:#x} ({}x{}, DXGI format {})",
                    metal.mtl_texture,
                    metal.width,
                    metal.height,
                    metal.dxgi_format
                );
                // Storage the application replaced is no longer needed.
                state.gpu.imports.retain(|i| i.gl != texture);
                state.gpu.imports.push(Import {
                    gl: texture,
                    mtl_texture: metal.mtl_texture,
                    format: metal.dxgi_format,
                    texture: imported,
                });
                Some(state.gpu.imports.len() - 1)
            }
            Err(hr) => disable(state, format!("DXMT import failed: {hr:?}")),
        }
    }

    /// Copies the submitted texture's region into `dst`'s array slice `dst_slice`,
    /// upright unless the bounds invert it (`flip` false). Returns the copied extent,
    /// or None after logging why.
    #[allow(clippy::too_many_arguments)]
    pub fn copy(
        &self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        eye: vr::EVREye,
        texture: GLuint,
        bounds: vr::VRTextureBounds_t,
        flip: bool,
        dst: &ID3D11Texture2D,
        dst_slice: u32,
    ) -> Option<xr::Extent2Di> {
        let started = std::time::Instant::now();
        let mut state = self.state.lock().unwrap();
        if let Some(i) = self.gpu_import(&mut state, device, texture) {
            let (width, height, _) = self.gl.texture_info(texture);
            let region = region_from_bounds(width, height, bounds);
            if region.width <= 0 || region.height <= 0 {
                return None;
            }
            // The copy runs on DXMT's queue: GL's rendering must be complete first.
            let waiting = std::time::Instant::now();
            unsafe { (self.gl.finish)() };
            state.gl_wait += waiting.elapsed();
            let src_box = D3D11_BOX {
                left: region.x as u32,
                top: region.y as u32,
                front: 0,
                right: (region.x + region.width) as u32,
                bottom: (region.y + region.height) as u32,
                back: 1,
            };
            let src = state.gpu.imports[i].texture.clone();
            if flip {
                if state.gpu.flip.is_none() {
                    match FlipCopy::new(device) {
                        Ok(f) => state.gpu.flip = Some(f),
                        Err(error) => {
                            log::warn!("no flipping copy ({error}); using CPU copies");
                            state.gpu.disabled = true;
                            drop(state);
                            return self.copy(device, context, eye, texture, bounds, flip, dst, dst_slice);
                        }
                    }
                }
                let flipper = state.gpu.flip.as_mut().unwrap();
                if let Err(error) = flipper.draw(device, context, &src, &src_box, dst, dst_slice) {
                    log::error!("OpenGL flipping copy failed: {error}");
                    return None;
                }
            } else {
                unsafe { context.CopySubresourceRegion(dst, dst_slice, 0, 0, 0, &src, 0, Some(&src_box)) };
            }
            let slot = eye as usize & 1;
            state.eye_copies[slot] += 1;
            let n = state.eye_copies[slot];
            if let Some(dir) = &self.capture_dir {
                if n % CAPTURE_INTERVAL == 0 && n / CAPTURE_INTERVAL <= CAPTURES_PER_EYE {
                    let eye_name = if slot == 0 { "left" } else { "right" };
                    if let Some(pixels) = read_back(device, context, &src, &src_box) {
                        capture(
                            dir.clone(),
                            format!("gl-{eye_name}-{}.png", n / CAPTURE_INTERVAL),
                            pixels,
                            region.width as u32,
                            region.height as u32,
                            flip,
                        );
                    }
                }
            }
            state.copies += 1;
            state.copy_time += started.elapsed();
            state.copy_bytes += (region.width * region.height * 4) as usize;
            self.report(&mut state, if flip { "GPU flipping copy" } else { "Metal blit" });
            return Some(xr::Extent2Di {
                width: region.width,
                height: region.height,
            });
        }
        drop(state);
        let (staged, src_box) = self.stage(device, context, eye, texture, bounds, flip)?;
        let staged = std::mem::ManuallyDrop::new(unsafe { ID3D11Texture2D::from_raw(staged) });
        unsafe { context.CopySubresourceRegion(dst, dst_slice, 0, 0, 0, &*staged, 0, Some(&src_box)) };
        Some(xr::Extent2Di {
            width: (src_box.right - src_box.left) as i32,
            height: (src_box.bottom - src_box.top) as i32,
        })
    }

    fn report(&self, state: &mut State, what: &str) {
        if state.copies == 450 {
            let per_copy = |d: std::time::Duration| d.as_secs_f64() * 1000.0 / state.copies as f64;
            let elapsed = state.window_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
            log::info!(
                "OpenGL {what}: {:.2} ms per eye ({:.2} ms waiting for the application's GL work), {:.1} MB per eye, {:.1} frames/s over {} copies; {} application GL errors cleared",
                per_copy(state.copy_time),
                per_copy(state.gl_wait),
                state.copy_bytes as f64 / state.copies as f64 / 1e6,
                if elapsed > 0.0 { state.copies as f64 / 2.0 / elapsed } else { 0.0 },
                state.copies,
                state.app_errors
            );
            state.copies = 0;
            state.copy_time = Default::default();
            state.gl_wait = Default::default();
            state.copy_bytes = 0;
            state.window_start = Some(std::time::Instant::now());
        } else if state.window_start.is_none() {
            state.window_start = Some(std::time::Instant::now());
        }
    }

    /// Reads the texture's region into a staging texture for `eye`, flipping its rows
    /// if asked. Returns that texture and the box to copy from it, or None after
    /// logging why.
    fn stage(
        &self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        eye: vr::EVREye,
        texture: GLuint,
        bounds: vr::VRTextureBounds_t,
        flip_rows: bool,
    ) -> Option<(*mut c_void, D3D11_BOX)> {
        let gl = &self.gl;
        let started = std::time::Instant::now();
        let mut state = self.state.lock().unwrap();
        let (width, height, _) = gl.texture_info(texture);
        let region = region_from_bounds(width, height, bounds);
        if region.width <= 0 || region.height <= 0 {
            return None;
        }
        let row_bytes = region.width as usize * 4;
        let size = row_bytes * region.height as usize;
        if state.pixels.len() < size {
            state.pixels.resize(size, 0);
        }

        let ok = unsafe {
            // Errors the application left pending are not ours; glGetError would report
            // them first. Count them, and leave GL without them.
            let mut pending = 0;
            while (gl.get_error)() != 0 && pending < 16 {
                pending += 1;
            }
            state.app_errors += pending;
            let mut failed: Option<(&str, GLenum)> = None;
            let mut check = |step: &'static str| {
                let error = (gl.get_error)();
                if error != 0 && failed.is_none() {
                    failed = Some((step, error));
                }
            };
            let previous_fbo = gl.integer(GL_READ_FRAMEBUFFER_BINDING) as GLuint;
            let previous_pack_buffer = gl.integer(GL_PIXEL_PACK_BUFFER_BINDING) as GLuint;
            let pack = [
                GL_PACK_ALIGNMENT,
                GL_PACK_ROW_LENGTH,
                GL_PACK_SKIP_ROWS,
                GL_PACK_SKIP_PIXELS,
            ]
            .map(|name| (name, gl.integer(name)));
            check("saving state");

            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, state.fbo);
            (gl.framebuffer_texture_2d)(
                GL_READ_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                texture,
                0,
            );
            check("attaching the texture");
            let previous_read_buffer = gl.integer(GL_READ_BUFFER);
            (gl.read_buffer)(GL_COLOR_ATTACHMENT0);
            check("selecting the read buffer");
            let complete = (gl.check_framebuffer_status)(GL_READ_FRAMEBUFFER);
            if previous_pack_buffer != 0 {
                (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, 0);
            }
            (gl.pixel_storei)(GL_PACK_ALIGNMENT, 4);
            (gl.pixel_storei)(GL_PACK_ROW_LENGTH, 0);
            (gl.pixel_storei)(GL_PACK_SKIP_ROWS, 0);
            (gl.pixel_storei)(GL_PACK_SKIP_PIXELS, 0);
            check("setting pack state");
            if complete == GL_FRAMEBUFFER_COMPLETE {
                (gl.read_pixels)(
                    region.x,
                    region.y,
                    region.width,
                    region.height,
                    GL_RGBA,
                    GL_UNSIGNED_BYTE,
                    state.pixels.as_mut_ptr().cast(),
                );
                check("reading pixels");
            }

            for (name, value) in pack {
                (gl.pixel_storei)(name, value);
            }
            if previous_pack_buffer != 0 {
                (gl.bind_buffer)(GL_PIXEL_PACK_BUFFER, previous_pack_buffer);
            }
            (gl.read_buffer)(previous_read_buffer as GLenum);
            (gl.framebuffer_texture_2d)(GL_READ_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, 0, 0);
            (gl.bind_framebuffer)(GL_READ_FRAMEBUFFER, previous_fbo);
            check("restoring state");
            let ok = complete == GL_FRAMEBUFFER_COMPLETE && failed.is_none();
            if !ok {
                state.failures += 1;
                if state.failures.is_power_of_two() {
                    log::error!(
                        "OpenGL readback of texture {texture} failed ({} times): framebuffer status {complete:#x}, {failed:?}",
                        state.failures
                    );
                }
            }
            ok
        };
        if !ok {
            return None;
        }

        // GL rows run bottom to top; D3D's run top to bottom.
        if flip_rows {
            self::flip_rows(&mut state.pixels[..size], row_bytes, region.height as usize);
        }

        let (width, height) = (region.width as u32, region.height as u32);
        let slot = eye as usize & 1;
        let reuse = state.staging[slot]
            .as_ref()
            .is_some_and(|s| s.width == width && s.height == height);
        if !reuse {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                ..Default::default()
            };
            let mut texture = None;
            if let Err(error) = unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) } {
                log::error!("creating a {width}x{height} staging texture failed: {error}");
                return None;
            }
            state.staging[slot] = Some(Staging {
                texture: texture?,
                width,
                height,
            });
        }
        let staging = &state.staging[slot].as_ref()?.texture;
        unsafe {
            context.UpdateSubresource(
                staging,
                0,
                None,
                state.pixels.as_ptr().cast(),
                row_bytes as u32,
                size as u32,
            );
        }
        let staging = staging.as_raw();
        state.eye_copies[slot] += 1;
        let n = state.eye_copies[slot];
        if let Some(dir) = &self.capture_dir {
            if n % CAPTURE_INTERVAL == 0 && n / CAPTURE_INTERVAL <= CAPTURES_PER_EYE {
                let eye_name = if slot == 0 { "left" } else { "right" };
                capture(
                    dir.clone(),
                    format!("gl-{eye_name}-{}.png", n / CAPTURE_INTERVAL),
                    state.pixels[..size].to_vec(),
                    width,
                    height,
                    false,
                );
            }
        }
        state.copies += 1;
        state.copy_time += started.elapsed();
        state.copy_bytes += size;
        self.report(&mut state, "CPU copy");
        Some((
            staging,
            D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: width,
                bottom: height,
                back: 1,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(u0: f32, v0: f32, u1: f32, v1: f32) -> vr::VRTextureBounds_t {
        vr::VRTextureBounds_t {
            uMin: u0,
            vMin: v0,
            uMax: u1,
            vMax: v1,
        }
    }

    #[test]
    fn side_by_side_right_eye() {
        let r = region_from_bounds(4000, 2000, bounds(0.5, 0.0, 1.0, 1.0));
        assert_eq!((r.x, r.y, r.width, r.height), (2000, 0, 2000, 2000));
    }

    #[test]
    fn inverted_bounds_cover_the_same_rows() {
        let r = region_from_bounds(100, 50, bounds(0.0, 1.0, 1.0, 0.0));
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 100, 50));
    }

    #[test]
    fn flips_rows() {
        let mut p = vec![1, 1, 2, 2, 3, 3];
        flip_rows(&mut p, 2, 3);
        assert_eq!(p, [3, 3, 2, 2, 1, 1]);
    }
}
