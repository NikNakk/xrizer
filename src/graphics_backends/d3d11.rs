use super::GraphicsBackend;
use openvr as vr;
use openxr as xr;

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::mem::ManuallyDrop;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BOX, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    };
    use windows::core::Interface;

    pub struct D3D11Data {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        images: Vec<*mut xr::sys::platform::ID3D11Texture2D>,
        format: u32,
    }

    impl D3D11Data {
        pub fn new(texture: &vr::Texture_t) -> Option<Self> {
            if texture.handle.is_null() {
                return None;
            }

            unsafe {
                let texture = ManuallyDrop::new(ID3D11Texture2D::from_raw(texture.handle));
                let mut device = None;
                texture.GetDevice(&mut device);
                let device = device?;

                let mut context = None;
                device.GetImmediateContext(&mut context);
                let context = context?;

                Some(Self {
                    device,
                    context,
                    images: Vec::new(),
                    format: 0,
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
                    format: 0,
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
            self.images = images;
            self.format = format;
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
            let dst = Self::borrow_texture(dst.cast::<c_void>());

            unsafe {
                self.context.CopySubresourceRegion(
                    &dst,
                    eye as u32,
                    0,
                    0,
                    0,
                    &src,
                    0,
                    Some(&src_box as *const D3D11_BOX),
                );
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
            let dst = Self::borrow_texture(dst.cast::<c_void>());

            unsafe {
                self.context.CopySubresourceRegion(
                    &dst,
                    0,
                    0,
                    0,
                    0,
                    &src,
                    0,
                    Some(&src_box as *const D3D11_BOX),
                );
            }

            extent
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
