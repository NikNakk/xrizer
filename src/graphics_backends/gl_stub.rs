use super::GraphicsBackend;
use openvr as vr;
use openxr as xr;

pub struct GlData;

impl GlData {
    pub(crate) fn new() -> Option<Self> {
        None
    }
}

impl GraphicsBackend for GlData {
    type Api = xr::OpenGL;
    type OpenVrTexture = u32;
    type NiceFormat = u32;

    fn to_nice_format(format: u32) -> Self::NiceFormat {
        format
    }

    fn session_create_info(&self) -> <Self::Api as xr::Graphics>::SessionCreateInfo {
        // Never used: GlData::new() returns None, so no OpenGL session is ever created.
        #[cfg(target_os = "windows")]
        {
            xr::opengl::SessionCreateInfo::Windows { h_dc: 0, h_glrc: 0 }
        }
        #[cfg(not(target_os = "windows"))]
        {
            xr::opengl::SessionCreateInfo::Wayland {
                display: std::ptr::null_mut(),
            }
        }
    }

    fn get_texture(_texture: &vr::Texture_t) -> Option<Self::OpenVrTexture> {
        None
    }

    fn swapchain_info_for_texture(
        &self,
        _texture: Self::OpenVrTexture,
        _bounds: vr::VRTextureBounds_t,
        _color_space: vr::EColorSpace,
    ) -> xr::SwapchainCreateInfo<Self::Api> {
        panic!("OpenGL OpenVR submissions are only supported on Linux")
    }

    fn store_swapchain_images(
        &mut self,
        _images: Vec<<Self::Api as xr::Graphics>::SwapchainImage>,
        _format: <Self::Api as xr::Graphics>::Format,
    ) {
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
        panic!("OpenGL OpenVR submissions are only supported on Linux")
    }

    fn copy_overlay_to_swapchain(
        &mut self,
        _texture: Self::OpenVrTexture,
        _bounds: vr::VRTextureBounds_t,
        _image_index: usize,
    ) -> xr::Extent2Di {
        panic!("OpenGL OpenVR submissions are only supported on Linux")
    }
}
