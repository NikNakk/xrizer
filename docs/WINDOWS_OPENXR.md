# Windows OpenXR / D3D11 port

This branch adds a Windows x64 path intended to run Windows OpenVR games through xrizer and a Windows OpenXR runtime.

The immediate target is Half-Life: Alyx under Wine/DXMT on macOS, using the Windows OpenXR bridge from the macOS Monado port.

## Architecture

```text
Windows OpenVR game (Alyx)
        |
        | OpenVR
        v
openvr_api.dll (xrizer, Windows x64)
        |
        | OpenXR + XR_KHR_D3D11_enable
        v
Windows OpenXR loader
        |
        v
libopenxr_monado.dll
        |
        | Wine/DXMT + IOSurface transport
        v
native macOS Monado service
```

xrizer does not need to know about Metal or IOSurface. It consumes the game's D3D11 texture and submits through ordinary D3D11 OpenXR swapchains. The existing Monado Windows bridge remains responsible for the macOS transport.

## Current branch functionality

- Windows x64 builds no longer pull in the Linux GLX implementation.
- `cargo xbuild` packages the Windows cdylib as:
  - `bin/vrclient_x64.dll`
  - `openvr_api.dll`
- The DLL exports the modern drop-in OpenVR client entry points, including:
  - `VR_InitInternal2`
  - `VR_InitInternal`
  - `VR_ShutdownInternal`
  - `VR_GetGenericInterface`
  - `VR_IsInterfaceVersionValid`
  - `VR_GetInitToken`
- `TextureType_DirectX` selects a D3D11 graphics backend.
- Windows OpenXR instances enable `XR_KHR_D3D11_enable`.
- Temporary pre-submit sessions use D3D11 on Windows rather than Vulkan.
- D3D11 submissions copy the requested OpenVR texture bounds into the appropriate eye layer of the OpenXR swapchain image.
- DirectX output-device queries return the OpenXR runtime's D3D11 adapter LUID; legacy DXGI adapter queries use adapter 0.

## Cross-build on Apple Silicon macOS

Install the Windows GNU toolchain if needed:

```zsh
brew install mingw-w64
rustup target add x86_64-pc-windows-gnu
```

Then:

```zsh
cd ~/Code/xrizer
git checkout windows-openxr-d3d11

XRIZER_VERSION=macos-dev \
cargo xbuild \
  --release \
  --target x86_64-pc-windows-gnu \
  --no-default-features
```

Expected drop-in DLL:

```text
target/x86_64-pc-windows-gnu/release/openvr_api.dll
```

Expected runtime DLL:

```text
target/x86_64-pc-windows-gnu/release/bin/vrclient_x64.dll
```

## First Alyx test

Back up the Alyx copy of `openvr_api.dll`, then replace it with the xrizer build.

Use the existing Monado Wine/DXMT launcher and OpenXR runtime registration. Do not use the legacy Unity OpenVR proxy.

Enable verbose logging for the first run, for example:

```zsh
RUST_LOG='xrizer=trace,openvr_calls=trace'
```

The first milestone is:

1. Alyx loads the xrizer `openvr_api.dll`.
2. Modern interfaces including `IVRSystem_026`, `IVROverlay_028`, and `IVRInput_011` are returned successfully.
3. xrizer creates its temporary D3D11 OpenXR session.
4. On the first `TextureType_DirectX` submit, xrizer restarts onto the game's D3D11 device.
5. An OpenXR D3D11 swapchain is created and the first eye textures are submitted to Monado.

Controllers are not required for this milestone.

## Known unverified areas

This is an initial bring-up and still needs a real Windows compile and runtime test.

In particular:

- The Windows build has not yet completed in GitHub Actions on this fork.
- The temporary D3D11 device currently uses the default hardware adapter. If the OpenXR runtime requires a specific adapter, device creation should be changed to select the adapter by the LUID returned by `graphics_requirements::<openxr::D3D11>()`.
- The initial copy path assumes the common Alyx case of a compatible D3D11 source/swapchain format and sample layout.
- MSAA resolve, incompatible/typeless format conversion, texture-array source selection, and unusual flipped bounds should be added only if observed.
- Overlay D3D11 copying currently targets array layer 0.
- Controller/input behavior still depends on xrizer's existing OpenVR-to-OpenXR mappings and the controller support exposed by Monado.

## CI

The branch adds a `windows-build` job on `windows-latest` which builds the runtime, runs tests, verifies the drop-in exports indirectly through the integration tests, and uploads both Windows DLL names.

If a new fork has GitHub Actions disabled, enable Actions for the repository before relying on this job.
