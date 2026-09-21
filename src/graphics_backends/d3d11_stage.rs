use crate::stage::{StageAsset, StageVertex, projection_matrix, view_matrix};
use openxr as xr;
use std::ffi::c_void;
use std::mem::size_of;
use windows::Win32::Graphics::Direct3D::{
    D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D::Fxc::{
    D3DCOMPILE_ENABLE_STRICTNESS, D3DCompile,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, s};

const STAGE_HLSL: &str = r#"
cbuffer StageConstants : register(b0)
{
    column_major float4x4 model;
    column_major float4x4 view_projection;
    float4 primary_color;
    float4 secondary_color;
    float4 effects; // x=greyscale, y=vignette inner, z=vignette outer, w=fresnel (reserved)
};

struct VSInput
{
    float3 position : POSITION;
    float2 tex_coord : TEXCOORD0;
};

struct VSOutput
{
    float4 position : SV_POSITION;
    float2 tex_coord : TEXCOORD0;
    float3 world_position : TEXCOORD1;
};

VSOutput VSMain(VSInput input)
{
    VSOutput output;
    float4 world = mul(model, float4(input.position, 1.0));
    output.position = mul(view_projection, world);
    output.tex_coord = input.tex_coord;
    output.world_position = world.xyz;
    return output;
}

Texture2D stage_texture : register(t0);
SamplerState stage_sampler : register(s0);

float4 PSMain(VSOutput input) : SV_TARGET
{
    float4 color = stage_texture.Sample(stage_sampler, input.tex_coord);

    if (effects.x > 0.5)
    {
        float luma = dot(color.rgb, float3(0.2126, 0.7152, 0.0722));
        color.rgb = luma.xxx;
    }

    color *= primary_color;

    if (effects.z > effects.y && effects.z > 0.0)
    {
        float distance_from_origin = length(input.world_position);
        float vignette = saturate(
            (distance_from_origin - effects.y) / max(effects.z - effects.y, 0.0001)
        );
        color = lerp(color, secondary_color, vignette);
    }

    return color;
}
"#;

#[repr(C)]
struct StageConstants {
    model: [[f32; 4]; 4],
    view_projection: [[f32; 4]; 4],
    primary_color: [f32; 4],
    secondary_color: [f32; 4],
    effects: [f32; 4],
}

pub struct StageRenderer {
    stage_id: u64,
    vertex_buffer: ID3D11Buffer,
    index_buffer: ID3D11Buffer,
    index_count: u32,
    constant_buffer: ID3D11Buffer,
    texture_view: ID3D11ShaderResourceView,
    sampler: ID3D11SamplerState,
    vertex_shader: ID3D11VertexShader,
    pixel_shader: ID3D11PixelShader,
    input_layout: ID3D11InputLayout,
    rasterizer: ID3D11RasterizerState,
    depth_texture: Option<ID3D11Texture2D>,
    depth_view: Option<ID3D11DepthStencilView>,
    depth_extent: (u32, u32),
}

impl StageRenderer {
    pub fn new(device: &ID3D11Device, stage: &StageAsset) -> Result<Self, String> {
        let vs_blob = compile_shader(STAGE_HLSL, "VSMain", "vs_5_0")?;
        let ps_blob = compile_shader(STAGE_HLSL, "PSMain", "ps_5_0")?;
        let vs_bytes = blob_bytes(&vs_blob);
        let ps_bytes = blob_bytes(&ps_blob);

        let mut vertex_shader = None;
        let mut pixel_shader = None;
        unsafe {
            device
                .CreateVertexShader(vs_bytes, None::<&ID3D11ClassLinkage>, Some(&mut vertex_shader))
                .map_err(|e| format!("CreateVertexShader failed: {e}"))?;
            device
                .CreatePixelShader(ps_bytes, None::<&ID3D11ClassLinkage>, Some(&mut pixel_shader))
                .map_err(|e| format!("CreatePixelShader failed: {e}"))?;
        }

        let input_desc = [
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("POSITION"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32B32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 0,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
            D3D11_INPUT_ELEMENT_DESC {
                SemanticName: s!("TEXCOORD"),
                SemanticIndex: 0,
                Format: DXGI_FORMAT_R32G32_FLOAT,
                InputSlot: 0,
                AlignedByteOffset: 12,
                InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                InstanceDataStepRate: 0,
            },
        ];
        let mut input_layout = None;
        unsafe {
            device
                .CreateInputLayout(&input_desc, vs_bytes, Some(&mut input_layout))
                .map_err(|e| format!("CreateInputLayout failed: {e}"))?;
        }

        let vertex_buffer = create_buffer(
            device,
            bytes_of_slice(&stage.vertices),
            D3D11_BIND_VERTEX_BUFFER.0 as u32,
            D3D11_USAGE_IMMUTABLE,
        )?;
        let index_buffer = create_buffer(
            device,
            bytes_of_slice(&stage.indices),
            D3D11_BIND_INDEX_BUFFER.0 as u32,
            D3D11_USAGE_IMMUTABLE,
        )?;

        let constant_desc = D3D11_BUFFER_DESC {
            ByteWidth: size_of::<StageConstants>() as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let mut constant_buffer = None;
        unsafe {
            device
                .CreateBuffer(&constant_desc, None, Some(&mut constant_buffer))
                .map_err(|e| format!("CreateBuffer(stage constants) failed: {e}"))?;
        }

        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: stage.texture_width,
            Height: stage.texture_height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_IMMUTABLE,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let texture_data = D3D11_SUBRESOURCE_DATA {
            pSysMem: stage.texture_rgba.as_ptr() as *const c_void,
            SysMemPitch: stage.texture_width * 4,
            SysMemSlicePitch: stage.texture_width * stage.texture_height * 4,
        };
        let mut texture = None;
        unsafe {
            device
                .CreateTexture2D(&texture_desc, Some(&texture_data), Some(&mut texture))
                .map_err(|e| format!("CreateTexture2D(stage texture) failed: {e}"))?;
        }
        let texture = texture.ok_or("CreateTexture2D(stage texture) returned no texture")?;
        let mut texture_view = None;
        unsafe {
            device
                .CreateShaderResourceView(&texture, None, Some(&mut texture_view))
                .map_err(|e| format!("CreateShaderResourceView(stage texture) failed: {e}"))?;
        }

        let sampler_desc = D3D11_SAMPLER_DESC {
            Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
            AddressU: D3D11_TEXTURE_ADDRESS_WRAP,
            AddressV: D3D11_TEXTURE_ADDRESS_WRAP,
            AddressW: D3D11_TEXTURE_ADDRESS_WRAP,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D11_COMPARISON_NEVER,
            MinLOD: 0.0,
            MaxLOD: f32::MAX,
            ..Default::default()
        };
        let mut sampler = None;
        unsafe {
            device
                .CreateSamplerState(&sampler_desc, Some(&mut sampler))
                .map_err(|e| format!("CreateSamplerState(stage) failed: {e}"))?;
        }

        let rasterizer_desc = D3D11_RASTERIZER_DESC {
            FillMode: if stage.settings.wireframe {
                D3D11_FILL_WIREFRAME
            } else {
                D3D11_FILL_SOLID
            },
            CullMode: if stage.settings.backface_culling {
                D3D11_CULL_BACK
            } else {
                D3D11_CULL_NONE
            },
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        let mut rasterizer = None;
        unsafe {
            device
                .CreateRasterizerState(&rasterizer_desc, Some(&mut rasterizer))
                .map_err(|e| format!("CreateRasterizerState(stage) failed: {e}"))?;
        }

        if stage.settings.fresnel_strength != 0.0 {
            log::warn!(
                "stage override FresnelStrength={} is not yet implemented",
                stage.settings.fresnel_strength
            );
        }

        Ok(Self {
            stage_id: stage.id,
            vertex_buffer: vertex_buffer.ok_or("CreateBuffer(vertices) returned no buffer")?,
            index_buffer: index_buffer.ok_or("CreateBuffer(indices) returned no buffer")?,
            index_count: stage.indices.len() as u32,
            constant_buffer: constant_buffer
                .ok_or("CreateBuffer(stage constants) returned no buffer")?,
            texture_view: texture_view
                .ok_or("CreateShaderResourceView(stage texture) returned no view")?,
            sampler: sampler.ok_or("CreateSamplerState(stage) returned no sampler")?,
            vertex_shader: vertex_shader.ok_or("CreateVertexShader returned no shader")?,
            pixel_shader: pixel_shader.ok_or("CreatePixelShader returned no shader")?,
            input_layout: input_layout.ok_or("CreateInputLayout returned no layout")?,
            rasterizer: rasterizer.ok_or("CreateRasterizerState returned no state")?,
            depth_texture: None,
            depth_view: None,
            depth_extent: (0, 0),
        })
    }

    pub fn stage_id(&self) -> u64 {
        self.stage_id
    }

    pub fn render(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        stage: &StageAsset,
        views: &[xr::View; 2],
        destination: &ID3D11Texture2D,
        extent: xr::Extent2Di,
    ) -> Result<(), String> {
        let width = extent.width.max(1) as u32;
        let height = extent.height.max(1) as u32;
        self.ensure_depth(device, width, height)?;

        let mut destination_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { destination.GetDesc(&mut destination_desc) };
        if destination_desc.ArraySize < 2 {
            return Err(format!(
                "stage projection needs a two-layer swapchain, got array size {}",
                destination_desc.ArraySize
            ));
        }

        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: width as f32,
            Height: height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let vertex_buffers = [Some(self.vertex_buffer.clone())];
        let strides = [size_of::<StageVertex>() as u32];
        let offsets = [0u32];
        let constant_buffers = [Some(self.constant_buffer.clone())];
        let shader_resources = [Some(self.texture_view.clone())];
        let samplers = [Some(self.sampler.clone())];

        unsafe {
            context.IASetInputLayout(&self.input_layout);
            context.IASetVertexBuffers(
                0,
                1,
                Some(vertex_buffers.as_ptr()),
                Some(strides.as_ptr()),
                Some(offsets.as_ptr()),
            );
            context.IASetIndexBuffer(&self.index_buffer, DXGI_FORMAT_R32_UINT, 0);
            context.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(&self.vertex_shader, None);
            context.VSSetConstantBuffers(0, Some(&constant_buffers));
            context.PSSetShader(&self.pixel_shader, None);
            context.PSSetShaderResources(0, Some(&shader_resources));
            context.PSSetSamplers(0, Some(&samplers));
            context.RSSetState(&self.rasterizer);
            context.RSSetViewports(Some(&[viewport]));
        }

        for (eye, view) in views.iter().enumerate() {
            let rtv_desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: destination_desc.Format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                        MipSlice: 0,
                        FirstArraySlice: eye as u32,
                        ArraySize: 1,
                    },
                },
            };
            let mut rtv = None;
            unsafe {
                device
                    .CreateRenderTargetView(destination, Some(&rtv_desc), Some(&mut rtv))
                    .map_err(|e| format!("CreateRenderTargetView(stage eye {eye}) failed: {e}"))?;
            }
            let rtv = rtv.ok_or_else(|| format!("no RTV returned for stage eye {eye}"))?;

            let view_projection = projection_matrix(view.fov, 0.05, 1000.0) * view_matrix(view);
            let constants = StageConstants {
                model: stage.model_transform.to_cols_array_2d(),
                view_projection: view_projection.to_cols_array_2d(),
                primary_color: stage.settings.primary_color,
                secondary_color: stage.settings.secondary_color,
                effects: [
                    u32::from(stage.settings.greyscale) as f32,
                    stage.settings.vignette_inner_radius,
                    stage.settings.vignette_outer_radius,
                    stage.settings.fresnel_strength,
                ],
            };

            unsafe {
                context.UpdateSubresource(
                    &self.constant_buffer,
                    0,
                    None,
                    &constants as *const StageConstants as *const c_void,
                    0,
                    0,
                );
                context.OMSetRenderTargets(Some(&[Some(rtv.clone())]), self.depth_view.as_ref());
                context.ClearRenderTargetView(&rtv, &stage.settings.secondary_color);
                if let Some(depth_view) = self.depth_view.as_ref() {
                    context.ClearDepthStencilView(
                        depth_view,
                        D3D11_CLEAR_DEPTH.0 as u32,
                        1.0,
                        0,
                    );
                }
                context.DrawIndexed(self.index_count, 0, 0);
            }
        }

        unsafe {
            // Do not leave our stage resources bound when the application resumes rendering.
            context.PSSetShaderResources(0, Some(&[None]));
            context.ClearState();
            context.Flush();
        }

        Ok(())
    }

    fn ensure_depth(
        &mut self,
        device: &ID3D11Device,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        if self.depth_extent == (width, height) && self.depth_view.is_some() {
            return Ok(());
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_D32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|e| format!("CreateTexture2D(stage depth) failed: {e}"))?;
        }
        let texture = texture.ok_or("CreateTexture2D(stage depth) returned no texture")?;

        let mut view = None;
        unsafe {
            device
                .CreateDepthStencilView(&texture, None, Some(&mut view))
                .map_err(|e| format!("CreateDepthStencilView(stage) failed: {e}"))?;
        }

        self.depth_texture = Some(texture);
        self.depth_view = Some(view.ok_or("CreateDepthStencilView(stage) returned no view")?);
        self.depth_extent = (width, height);
        Ok(())
    }
}

fn create_buffer(
    device: &ID3D11Device,
    bytes: &[u8],
    bind_flags: u32,
    usage: D3D11_USAGE,
) -> Result<Option<ID3D11Buffer>, String> {
    if bytes.is_empty() {
        return Err("attempted to create an empty D3D11 stage buffer".into());
    }

    let desc = D3D11_BUFFER_DESC {
        ByteWidth: bytes.len() as u32,
        Usage: usage,
        BindFlags: bind_flags,
        ..Default::default()
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr() as *const c_void,
        ..Default::default()
    };
    let mut buffer = None;
    unsafe {
        device
            .CreateBuffer(&desc, Some(&initial), Some(&mut buffer))
            .map_err(|e| format!("CreateBuffer(stage) failed: {e}"))?;
    }
    Ok(buffer)
}

fn compile_shader(source: &str, entry: &str, target: &str) -> Result<ID3DBlob, String> {
    let entry = std::ffi::CString::new(entry).map_err(|e| e.to_string())?;
    let target = std::ffi::CString::new(target).map_err(|e| e.to_string())?;
    let source_name = s!("xrizer-stage.hlsl");
    let mut code = None;
    let mut errors = None;

    let result = unsafe {
        D3DCompile(
            source.as_ptr() as *const c_void,
            source.len(),
            source_name,
            None,
            None::<&ID3DInclude>,
            windows::core::PCSTR(entry.as_ptr() as *const u8),
            windows::core::PCSTR(target.as_ptr() as *const u8),
            D3DCOMPILE_ENABLE_STRICTNESS,
            0,
            &mut code,
            Some(&mut errors),
        )
    };

    if let Err(error) = result {
        let compiler_message = errors
            .as_ref()
            .map(blob_string)
            .unwrap_or_else(|| "<no compiler diagnostic>".into());
        return Err(format!(
            "D3DCompile({entry:?}, {target:?}) failed: {error}: {compiler_message}"
        ));
    }

    code.ok_or_else(|| format!("D3DCompile({entry:?}) returned no bytecode"))
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
    }
}

fn blob_string(blob: &ID3DBlob) -> String {
    String::from_utf8_lossy(blob_bytes(blob))
        .trim_end_matches('\0')
        .to_owned()
}

fn bytes_of_slice<T>(slice: &[T]) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            slice.as_ptr() as *const u8,
            std::mem::size_of_val(slice),
        )
    }
}
