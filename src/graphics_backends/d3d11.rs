use super::GraphicsBackend;
use openvr as vr;
use openxr as xr;

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use super::d3d11_stage::StageRenderer;
    use std::ffi::c_void;
    use std::mem::ManuallyDrop;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM_SRGB;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_RENDER_TARGET_VIEW_DESC,
        D3D11_RENDER_TARGET_VIEW_DESC_0, D3D11_RTV_DIMENSION_TEXTURE2D,
        D3D11_SDK_VERSION, D3D11_TEX2D_RTV, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
        ID3D11Texture2D,
    };
    use windows::core::Interface;

    static COPY_DIAGNOSTIC_COUNT: AtomicU32 = AtomicU32::new(0);

    fn env_enabled(name: &str) -> bool {
        std::env::var(name)
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes" | "on"))
    }

    pub struct D3D11Data {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        images: Vec<usize>,
        stage_renderer: Option<StageRenderer>,
    }

    impl D3D11Data {
        pub fn new(texture: &vr::Texture_t) -> Option<Self> {
            if texture.handle.is_null() {
                return None;
            }

            unsafe {
                let texture = ManuallyDrop::new(ID3D11Texture2D::from_raw(texture.handle));
                let device = texture.GetDevice().ok()?;

                let context = device.GetImmediateContext().ok()?;

                Some(Self {
                    device,
                    context,
                    images: Vec::new(),
                    stage_renderer: None,
                })
            }
        }

        pub fn new_temporary() -> Option<Self> {
            unsafe {
                let mut device = None;
                let mut context = None;
                if D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_FLAG(0),
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
                .is_err()
                {
                    return None;
                }

                Some(Self {
                    device: device?,
                    context: context?,
                    images: Vec::new(),
                    stage_renderer: None,
                })
            }
        }

        fn borrow_texture(handle: *mut c_void) -> ManuallyDrop<ID3D11Texture2D> {
            unsafe { ManuallyDrop::new(ID3D11Texture2D::from_raw(handle)) }
        }

        fn texture_desc(handle: *mut c_void) -> D3D11_TEXTURE2D_DESC {
            let texture = Self::borrow_texture(handle);
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc) };
            desc
        }

        fn rect_from_bounds(
            desc: &D3D11_TEXTURE2D_DESC,
            bounds: vr::VRTextureBounds_t,
        ) -> (D3D11_BOX, xr::Extent2Di) {
            let x0 = (bounds.uMin * desc.Width as f32).round() as i32;
            let x1 = (bounds.uMax * desc.Width as f32).round() as i32;
            let y0 = (bounds.vMin * desc.Height as f32).round() as i32;
            let y1 = (bounds.vMax * desc.Height as f32).round() as i32;

            let left = x0.min(x1).max(0) as u32;
            let right = x0.max(x1).clamp(0, desc.Width as i32) as u32;
            let top = y0.min(y1).max(0) as u32;
            let bottom = y0.max(y1).clamp(0, desc.Height as i32) as u32;

            (
                D3D11_BOX {
                    left,
                    top,
                    front: 0,
                    right,
                    bottom,
                    back: 1,
                },
                xr::Extent2Di {
                    width: right.saturating_sub(left) as i32,
                    height: bottom.saturating_sub(top) as i32,
                },
            )
        }

        fn cpu_copy_region(
            &self,
            src: &ID3D11Texture2D,
            src_desc: &D3D11_TEXTURE2D_DESC,
            src_box: &D3D11_BOX,
            dst: &ID3D11Texture2D,
            dst_subresource: u32,
            extent: xr::Extent2Di,
        ) -> Result<(), String> {
            let mut staging_desc = D3D11_TEXTURE2D_DESC::default();
            staging_desc.Width = extent.width.max(1) as u32;
            staging_desc.Height = extent.height.max(1) as u32;
            staging_desc.MipLevels = 1;
            staging_desc.ArraySize = 1;
            staging_desc.Format = src_desc.Format;
            staging_desc.SampleDesc.Count = 1;
            staging_desc.SampleDesc.Quality = 0;
            staging_desc.Usage = D3D11_USAGE_STAGING;
            staging_desc.BindFlags = 0;
            staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            staging_desc.MiscFlags = 0;

            let mut staging = None;
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))
                    .map_err(|err| format!("CreateTexture2D(staging) failed: {err}"))?;
            }
            let staging = staging.ok_or("CreateTexture2D(staging) returned no texture")?;

            unsafe {
                self.context.CopySubresourceRegion(
                    &staging,
                    0,
                    0,
                    0,
                    0,
                    src,
                    0,
                    Some(src_box as *const D3D11_BOX),
                );
            }

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(|err| format!("Map(staging) failed: {err}"))?;
            }

            if mapped.pData.is_null() {
                unsafe { self.context.Unmap(&staging, 0) };
                return Err("Map(staging) returned a null data pointer".into());
            }

            if env_enabled("XRIZER_D3D11_DIAGNOSTICS") {
                let read_pixel = |x: usize, y: usize| {
                    let offset = y * mapped.RowPitch as usize + x * 4;
                    unsafe {
                        let ptr = (mapped.pData as *const u8).add(offset);
                        [*ptr, *ptr.add(1), *ptr.add(2), *ptr.add(3)]
                    }
                };

                let center = read_pixel(
                    (staging_desc.Width / 2) as usize,
                    (staging_desc.Height / 2) as usize,
                );

                let xs = [
                    staging_desc.Width / 8,
                    staging_desc.Width / 4,
                    staging_desc.Width / 2,
                    staging_desc.Width * 3 / 4,
                    staging_desc.Width * 7 / 8,
                ];
                let ys = [
                    staging_desc.Height / 8,
                    staging_desc.Height / 4,
                    staging_desc.Height / 2,
                    staging_desc.Height * 3 / 4,
                    staging_desc.Height * 7 / 8,
                ];

                let mut sampled = 0u32;
                let mut non_black = 0u32;
                let mut min_rgb = [u8::MAX; 3];
                let mut max_rgb = [0u8; 3];
                for y in ys {
                    for x in xs {
                        let p = read_pixel(x as usize, y as usize);
                        sampled += 1;
                        if p[0] != 0 || p[1] != 0 || p[2] != 0 {
                            non_black += 1;
                        }
                        for channel in 0..3 {
                            min_rgb[channel] = min_rgb[channel].min(p[channel]);
                            max_rgb[channel] = max_rgb[channel].max(p[channel]);
                        }
                    }
                }

                log::info!(
                    "D3D11 CPU-copy sample: dst_subresource={dst_subresource} row_pitch={} \
                     center_rgba={center:?} grid_non_black={non_black}/{sampled} \
                     grid_rgb_min={min_rgb:?} grid_rgb_max={max_rgb:?}",
                    mapped.RowPitch
                );
            }

            unsafe {
                self.context.UpdateSubresource(
                    dst,
                    dst_subresource,
                    None,
                    mapped.pData,
                    mapped.RowPitch,
                    mapped.DepthPitch,
                );
                self.context.Unmap(&staging, 0);
            }

            Ok(())
        }
    }

    impl GraphicsBackend for D3D11Data {
        type Api = xr::D3D11;
        type OpenVrTexture = *mut c_void;
        type NiceFormat = u32;

        fn to_nice_format(format: u32) -> Self::NiceFormat {
            format
        }

        fn session_create_info(&self) -> <Self::Api as xr::Graphics>::SessionCreateInfo {
            xr::d3d::SessionCreateInfoD3D11 {
                device: self.device.as_raw() as _,
            }
        }

        fn get_texture(texture: &vr::Texture_t) -> Option<Self::OpenVrTexture> {
            (!texture.handle.is_null()).then_some(texture.handle)
        }

        fn swapchain_info_for_texture(
            &self,
            texture: Self::OpenVrTexture,
            bounds: vr::VRTextureBounds_t,
            _color_space: vr::EColorSpace,
        ) -> xr::SwapchainCreateInfo<Self::Api> {
            let desc = Self::texture_desc(texture);
            let (_, extent) = Self::rect_from_bounds(&desc, bounds);

            xr::SwapchainCreateInfo {
                create_flags: xr::SwapchainCreateFlags::EMPTY,
                usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                    | xr::SwapchainUsageFlags::TRANSFER_DST,
                format: desc.Format.0 as u32,
                sample_count: desc.SampleDesc.Count.max(1),
                width: extent.width.max(1) as u32,
                height: extent.height.max(1) as u32,
                face_count: 1,
                array_size: 2,
                mip_count: 1,
            }
        }

        fn store_swapchain_images(
            &mut self,
            images: Vec<<Self::Api as xr::Graphics>::SwapchainImage>,
            format: u32,
        ) {
            self.images = images.into_iter().map(|image| image as usize).collect();
            let _ = format;
        }

        fn copy_texture_to_swapchain(
            &self,
            eye: vr::EVREye,
            texture: Self::OpenVrTexture,
            _color_space: vr::EColorSpace,
            bounds: vr::VRTextureBounds_t,
            image_index: usize,
            _submit_flags: vr::EVRSubmitFlags,
        ) -> xr::Extent2Di {
            let Some(&dst) = self.images.get(image_index) else {
                return xr::Extent2Di { width: 0, height: 0 };
            };

            let src_desc = Self::texture_desc(texture);
            let (src_box, extent) = Self::rect_from_bounds(&src_desc, bounds);
            let src = Self::borrow_texture(texture);
            let dst = Self::borrow_texture(dst as *mut c_void);

            if env_enabled("XRIZER_D3D11_DIAGNOSTICS") {
                let n = COPY_DIAGNOSTIC_COUNT.fetch_add(1, Ordering::Relaxed);
                if n < 16 {
                    let mut dst_desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { dst.GetDesc(&mut dst_desc) };

                    let src_device = unsafe { src.GetDevice().ok() };
                    let dst_device = unsafe { dst.GetDevice().ok() };
                    let src_device_ptr = src_device
                        .as_ref()
                        .map(|device| device.as_raw())
                        .unwrap_or(std::ptr::null_mut());
                    let dst_device_ptr = dst_device
                        .as_ref()
                        .map(|device| device.as_raw())
                        .unwrap_or(std::ptr::null_mut());
                    let backend_device_ptr = self.device.as_raw();

                    log::info!(
                        "D3D11 eye copy #{n}: eye={eye:?} image_index={image_index} \
                         src={:p} src_device={:p} backend_device={:p} same_src_device={} \
                         src={}x{} fmt={} mips={} array={} samples={} quality={} usage={:?} bind=0x{:x} cpu=0x{:x} misc=0x{:x} \
                         bounds=({:.6},{:.6})-({:.6},{:.6}) \
                         box=({},{})->({},{}) extent={}x{} \
                         dst={:p} dst_device={:p} same_dst_device={} \
                         dst={}x{} fmt={} mips={} array={} samples={} quality={}",
                        texture,
                        src_device_ptr,
                        backend_device_ptr,
                        src_device_ptr == backend_device_ptr,
                        src_desc.Width,
                        src_desc.Height,
                        src_desc.Format.0,
                        src_desc.MipLevels,
                        src_desc.ArraySize,
                        src_desc.SampleDesc.Count,
                        src_desc.SampleDesc.Quality,
                        src_desc.Usage,
                        src_desc.BindFlags,
                        src_desc.CPUAccessFlags,
                        src_desc.MiscFlags,
                        bounds.uMin,
                        bounds.vMin,
                        bounds.uMax,
                        bounds.vMax,
                        src_box.left,
                        src_box.top,
                        src_box.right,
                        src_box.bottom,
                        extent.width,
                        extent.height,
                        dst.as_raw(),
                        dst_device_ptr,
                        dst_device_ptr == backend_device_ptr,
                        dst_desc.Width,
                        dst_desc.Height,
                        dst_desc.Format.0,
                        dst_desc.MipLevels,
                        dst_desc.ArraySize,
                        dst_desc.SampleDesc.Count,
                        dst_desc.SampleDesc.Quality,
                    );

                    if src_device_ptr != backend_device_ptr {
                        log::warn!(
                            "D3D11 submitted texture belongs to a different device than the OpenXR session"
                        );
                    }
                }
            }

            if env_enabled("XRIZER_D3D11_TEST_CLEAR_SOURCE") && eye == vr::EVREye::Left {
                let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                    },
                };
                let mut rtv = None;
                unsafe {
                    match self
                        .device
                        .CreateRenderTargetView(&*src, Some(&desc), Some(&mut rtv))
                    {
                        Ok(()) => {
                            if let Some(rtv) = rtv {
                                self.context
                                    .ClearRenderTargetView(&rtv, &[1.0, 0.0, 1.0, 1.0]);
                                if env_enabled("XRIZER_D3D11_DIAGNOSTICS") {
                                    log::info!(
                                        "D3D11 source diagnostic clear: cleared submitted texture to magenta"
                                    );
                                }
                            } else {
                                log::warn!(
                                    "D3D11 source diagnostic clear succeeded without returning an RTV"
                                );
                            }
                        }
                        Err(err) => {
                            log::warn!(
                                "D3D11 source diagnostic clear failed to create typed RTV: {err}"
                            );
                        }
                    }
                }
            }

            if env_enabled("XRIZER_D3D11_PREFLUSH") {
                unsafe { self.context.Flush() };
            }

            if env_enabled("XRIZER_D3D11_CPU_COPY") {
                if let Err(err) = self.cpu_copy_region(
                    &src,
                    &src_desc,
                    &src_box,
                    &dst,
                    eye as u32,
                    extent,
                ) {
                    log::warn!("D3D11 CPU-copy diagnostic failed: {err}");
                }
            } else {
                unsafe {
                    self.context.CopySubresourceRegion(
                        &*dst,
                        eye as u32,
                        0,
                        0,
                        0,
                        &*src,
                        0,
                        Some(&src_box as *const D3D11_BOX),
                    );
                }
            }

            unsafe {
                if env_enabled("XRIZER_D3D11_TEST_CLEAR") && eye == vr::EVREye::Right {
                    let mut rtv = None;
                    match self
                        .device
                        .CreateRenderTargetView(&*dst, None, Some(&mut rtv))
                    {
                        Ok(()) => {
                            if let Some(rtv) = rtv {
                                // Loud magenta: unmistakable if the array swapchain reaches the HMD.
                                self.context
                                    .ClearRenderTargetView(&rtv, &[1.0, 0.0, 1.0, 1.0]);
                            } else {
                                log::warn!(
                                    "D3D11 diagnostic test clear succeeded without returning an RTV"
                                );
                            }
                        }
                        Err(err) => {
                            log::warn!("D3D11 diagnostic test clear failed to create RTV: {err}");
                        }
                    }
                }

                if env_enabled("XRIZER_D3D11_FLUSH") {
                    self.context.Flush();
                }
            }

            extent
        }

        fn copy_overlay_to_swapchain(
            &mut self,
            texture: Self::OpenVrTexture,
            bounds: vr::VRTextureBounds_t,
            image_index: usize,
        ) -> xr::Extent2Di {
            let Some(&dst) = self.images.get(image_index) else {
                return xr::Extent2Di { width: 0, height: 0 };
            };

            let src_desc = Self::texture_desc(texture);
            let (src_box, extent) = Self::rect_from_bounds(&src_desc, bounds);
            let src = Self::borrow_texture(texture);
            let dst = Self::borrow_texture(dst as *mut c_void);

            unsafe {
                self.context.CopySubresourceRegion(
                    &*dst,
                    0,
                    0,
                    0,
                    0,
                    &*src,
                    0,
                    Some(&src_box as *const D3D11_BOX),
                );
            }

            extent
        }

        fn render_stage(
            &mut self,
            stage: &crate::stage::StageAsset,
            views: &[xr::View; 2],
            image_index: usize,
            extent: xr::Extent2Di,
        ) -> Result<(), String> {
            let Some(&dst) = self.images.get(image_index) else {
                return Err(format!("stage swapchain image index {image_index} is unavailable"));
            };
            let dst = Self::borrow_texture(dst as *mut c_void);

            if self
                .stage_renderer
                .as_ref()
                .is_none_or(|renderer| renderer.stage_id() != stage.id)
            {
                log::info!(
                    "creating D3D11 stage renderer for {:?}: {} vertices, {} indices, texture {}x{}",
                    stage.source_path,
                    stage.vertices.len(),
                    stage.indices.len(),
                    stage.texture_width,
                    stage.texture_height,
                );
                self.stage_renderer = Some(StageRenderer::new(&self.device, stage)?);
            }

            self.stage_renderer
                .as_mut()
                .expect("stage renderer was just initialized")
                .render(&self.device, &self.context, stage, views, &dst, extent)
        }

        fn clear_stage(&mut self) {
            self.stage_renderer = None;
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod platform {
    use super::*;

    pub enum DummyD3D11 {}

    impl xr::Graphics for DummyD3D11 {
        type Requirements = ();
        type SessionCreateInfo = ();
        type Format = u32;
        type SwapchainImage = u64;

        fn raise_format(x: i64) -> Self::Format {
            x as u32
        }

        fn lower_format(x: Self::Format) -> i64 {
            x as i64
        }

        fn requirements(
            _instance: &xr::Instance,
            _system: xr::SystemId,
        ) -> xr::Result<Self::Requirements> {
            unreachable!("D3D11 is unavailable on non-Windows targets")
        }

        unsafe fn create_session(
            _instance: &xr::Instance,
            _system: xr::SystemId,
            _info: &Self::SessionCreateInfo,
        ) -> xr::Result<xr::sys::Session> {
            unreachable!("D3D11 is unavailable on non-Windows targets")
        }

        fn enumerate_swapchain_images(
            _swapchain: &xr::Swapchain<Self>,
        ) -> xr::Result<Vec<Self::SwapchainImage>> {
            unreachable!("D3D11 is unavailable on non-Windows targets")
        }
    }

    pub struct D3D11Data;

    impl D3D11Data {
        pub fn new(_texture: &vr::Texture_t) -> Option<Self> {
            None
        }
    }

    impl GraphicsBackend for D3D11Data {
        type Api = DummyD3D11;
        type OpenVrTexture = *mut std::ffi::c_void;
        type NiceFormat = u32;

        fn to_nice_format(format: u32) -> Self::NiceFormat {
            format
        }

        fn session_create_info(&self) -> <Self::Api as xr::Graphics>::SessionCreateInfo {}

        fn get_texture(_texture: &vr::Texture_t) -> Option<Self::OpenVrTexture> {
            None
        }

        fn swapchain_info_for_texture(
            &self,
            _texture: Self::OpenVrTexture,
            _bounds: vr::VRTextureBounds_t,
            _color_space: vr::EColorSpace,
        ) -> xr::SwapchainCreateInfo<Self::Api> {
            unreachable!("D3D11 is unavailable on non-Windows targets")
        }

        fn store_swapchain_images(
            &mut self,
            _images: Vec<<Self::Api as xr::Graphics>::SwapchainImage>,
            _format: <Self::Api as xr::Graphics>::Format,
        ) {
            unreachable!("D3D11 is unavailable on non-Windows targets")
        }

        fn copy_texture_to_swapchain(
            &self,
            _eye: vr::EVREye,
            _texture: Self::OpenVrTexture,
            _color_space: vr::EColorSpace,
            _bounds: vr::VRTextureBounds_t,
            _image_index: usize,
            _submit_flags: vr::EVRSubmitFlags,
        ) -> xr::Extent2Di {
            unreachable!("D3D11 is unavailable on non-Windows targets")
        }

        fn copy_overlay_to_swapchain(
            &mut self,
            _texture: Self::OpenVrTexture,
            _bounds: vr::VRTextureBounds_t,
            _image_index: usize,
        ) -> xr::Extent2Di {
            unreachable!("D3D11 is unavailable on non-Windows targets")
        }
    }
}

pub use platform::D3D11Data;
