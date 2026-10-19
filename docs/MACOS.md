# macOS port (proof of concept, Vulkan)

This branch builds xrizer as a native macOS OpenVR runtime. Started 2026-10-19; it has
been compiled and type-checked but **not yet run on a Mac**.

The goal is to give native macOS OpenVR applications a runtime on top of a native OpenXR
runtime (Monado). The first applications in view are:

- **Valve's Linux ARM64 build of Half-Life: Alyx**, run natively on macOS (as VeRantyx's
  `native` branch does with Klepton). It renders with Vulkan, through MoltenVK.
- **Later, Windows games under Wine**, through a port of Proton's `vrclient_x64` bridge. That
  bridge hands this native xrizer the game's frames, so xrizer's own work is native code
  rather than translated x86.

```text
native OpenVR app (Vulkan, via MoltenVK)
        |
        | OpenVR (bin/osx32/vrclient.dylib)
        v
xrizer, native arm64
        |
        | OpenXR + XR_KHR_vulkan_enable, on the app's own VkDevice
        v
Khronos loader -> Monado client -> monado-service -> PS VR2 / simulated HMD
```

The proof of concept keeps **Vulkan throughout**: xrizer's existing Vulkan backend
is unchanged, and Monado's macOS client already imports Vulkan swapchain images backed
by Metal. Whether a Metal backend is worth adding comes afterwards.

## What changed

- `build.rs`: macOS is a supported platform. openvr_api on macOS looks for
  `bin/osx32/vrclient.dylib` on Intel and Apple silicon alike (`PLATSUBDIR` in
  `pathtools_public.h`), so `xbuild` lays the runtime out that way.
- `xbuild`: recognises the `.dylib` artifact.
- OpenGL submissions are Linux-only (GLX). macOS uses the same stub as Windows.
- The app name comes from `current_exe()`. `/proc/self/exe` is Linux-only.
- The panic dialog is off on macOS. AppKit windows can only be opened on the main thread,
  and panics come from the game's threads. The log is the report.
- **Vulkan on MoltenVK** (`graphics_backends/vulkan.rs`, module `portability`):
  - **Which Vulkan library:** xrizer drives the app's instance and device with function
    pointers it loads itself, so it must use the same library the app did. It first takes
    `vkGetInstanceProcAddr` from the process (the app's loader, or MoltenVK linked directly).
    Only if there is none does it search: `XRIZER_VULKAN_LIBRARY`, then the usual Homebrew
    and system paths.
  - **The temporary session** (before the app submits) asks for
    `VK_KHR_portability_enumeration` and enables `VK_KHR_portability_subset` when the device
    offers it, as MoltenVK requires.
- **OpenXR loader:** found through `XRIZER_OPENXR_LOADER`, then the usual paths. The
  Khronos loader is not on macOS's default library search path.
- `tools/macos/vulkan_submit_probe.cpp`: a miniature OpenVR scene app for testing without a
  game (below).
- `.github/workflows/macos.yml`: builds, checks the packaged `vrclient.dylib` and its
  `VRClientCoreFactory` export, runs the unit tests and clippy, and builds the probe on
  macOS arm64.

Unchanged: input, the compositor and everything else. They are platform-neutral Rust.

Three input tests (`action_origins_match_pose_and_skeleton_hands`,
`non_hand_restriction_activates_only_secondary_set_for_hands`,
`restricted_action_set_activates_secondary_set_for_other_hand`) fail on Linux on
`windows-openxr-d3d11` before this branch too. The macOS workflow skips them.

## Build

```sh
brew install shaderc vulkan-headers vulkan-loader molten-vk
cargo xbuild --release        # target/release/bin/osx32/vrclient.dylib
tools/macos/build-probe.sh    # target/vulkan_submit_probe
```

To use the build as the OpenVR runtime, copy `target/release/openvrpaths.vrpath` to
`~/Library/Application Support/.openvr/openvrpaths.vrpath`. That is openvr_api's macOS path
registry, not `~/.config/openvr` as on Linux.

## First test: the probe with the simulated HMD

```sh
# Monado, simulated HMD in a desktop window (claude/wsi-fragment-output or later)
SIMULATED_DESKTOP=1 monado-service &

XR_RUNTIME_JSON=/path/to/monado/build/openxr_monado-dev.json \
XRIZER_OPENXR_LOADER=/path/to/libopenxr_loader.dylib \
RUST_LOG=xrizer=debug \
  target/vulkan_submit_probe target/release/bin/osx32/vrclient.dylib 900
```

- **Expected:** red in the left eye and blue in the right, both pulsing, and the printed head
  pose following the trackpad.
- **The log:** `~/.local/state/xrizer/xrizer.txt` (or `$XDG_STATE_HOME/xrizer`).
- **What it covers:** loading as openvr_api does, Init and the interface versions, the
  extension lists xrizer reports to apps, `GetOutputDevice`, xrizer's temporary Vulkan
  session on MoltenVK, the switch to the app's device at the first Submit, and Monado's
  import of the swapchain images.

## Then

1. **Native Alyx (Linux ARM64 build).**
   - Alyx's `libopenvr_api.so` is a Linux library. Under Klepton its `dlopen` of
     `bin/linuxarm64/vrclient.so` has to reach this macOS `vrclient.dylib`, as Klepton's host
     already redirects `steamclient.so` to the Mac Steam client's `steamclient.dylib`.
     OpenVR's interfaces are C++ vtables with the same AArch64 calling convention on Linux
     and macOS, and OpenVR packs its structures to 4 bytes on both.
   - Alyx must create its VkInstance and VkDevice through the same MoltenVK as xrizer. The
     in-process `vkGetInstanceProcAddr` lookup above is for this case.
2. **The fork's Alyx work:**
   - From `windows-openxr-d3d11`: mailbox ready semantics, stage rendering and the input
     fixes. They are platform-neutral and already on this branch.
   - The later cleanup in the local checkout (session lock fix and following) still needs
     pushing; this branch is meant to rebase onto it.
3. **Decide on Metal.**
   - With Vulkan proven, compare against a Metal path: a `TextureType_Metal` backend on
     `XR_KHR_metal_enable`, as Monado's native clients use.
   - That matters most for the Wine vrclient bridge, where DXMT hands over Metal textures.
