# Selectable macOS Vulkan backends (2026-10-19)

MoltenVK stays the default. Nothing here installs an ICD, an OpenVR registry or
Monado. Use one native Vulkan loader for the application, Klepton and xrizer.
`XRIZER_VULKAN_LIBRARY` now fails on an invalid explicit path or a dispatcher
that disagrees with an already globally loaded `vkGetInstanceProcAddr`.
Locally scoped unrelated dispatchers cannot be detected by this symbol check;
the launcher contract is still necessary.

Build with `cargo build --locked` and `sh tools/macos/build-probe.sh`.
The macOS port's lockfile was stale; this branch refreshes it using locally
cached dependencies. Two existing unused stage-function warnings remain.
The fake Vulkan driver's two missing core extension enumeration functions are
implemented so macOS portability queries can run under unit tests.

With a separately running **matching build-tree** simulated Monado runtime:

```sh
export XR_RUNTIME_JSON=/path/to/monado-build/openxr_monado-dev.json
export XRIZER_OPENXR_LOADER=/path/to/libopenxr_loader.dylib
python3 tools/macos/run-vulkan-backend.py --output /tmp/probe-mvk -- \
  "$PWD/target/vulkan_submit_probe" "$PWD/target/debug/libxrizer.dylib" 180
python3 tools/macos/run-vulkan-backend.py --driver kosmickrisp \
  --icd /path/to/mesa-build/src/kosmickrisp/vulkan/kosmickrisp_mesa_devenv_icd.aarch64.json \
  --output /tmp/probe-kk -- \
  "$PWD/target/vulkan_submit_probe" "$PWD/target/debug/libxrizer.dylib" 180
```

Outputs must be new directories. `--loader` overrides the common loader path;
`--probe` overrides the census binary. The launcher verifies the observed driver
ID before running the command, records ICD/library/hash/loader/command/device,
and saves the complete offered extension list and application log. It sets
`VK_DRIVER_FILES`, `XRIZER_VULKAN_LIBRARY`, and patched Klepton's
`KL_VULKAN_LIBRARY`. No KosmicKrisp library is needed for the default launch.
Explicitly selecting a missing or wrong driver never falls back to MoltenVK.

`target/vulkan_submit_probe --driver-only` tests an ICD without OpenVR or OpenXR.
The submission probe requests Vulkan 1.2 and the available timeline semaphore
feature, logs the selected physical device, and fails on OpenVR submission
errors. Its CPU frame percentiles exclude the first 30 frames; first handoff is
not a measured physical display timestamp. The temporary session uses Vulkan
1.2 on macOS to avoid MoltenVK lowering device properties to Vulkan 1.0.

Tested MoltenVK passes stereo-array submissions with two Monado legacy-path
fixes (unsupported FD requirements and Metal-object bookkeeping). Claude is
also fixing those; use his integration candidate rather than merging duplicate
fixes from this experiment. KosmicKrisp/Mesa 0913fce718 builds and passes the ICD
census but cannot create the required OpenXR device: `VK_EXT_metal_objects` is
absent. This is a tested interoperability blocker, not a working KK XR backend.
The existing xrizer Vulkan path retains its two GPU eye copies into the OpenXR
array and the Monado legacy CPU handoff fallback; this change adds neither
another compositor nor another texture copy.

Foveation: `IVRSystem_026`'s two eye-tracked foveation-centre calls currently
return false in xrizer. Observe actual Alyx calls before implementing gaze
bridging or claiming a GPU identity/runtime gate. Klepton tracing is prepared
separately, with no evidence from Alyx yet because no native game files exist.
