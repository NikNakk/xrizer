# Windows OpenXR / D3D11 port

Status reviewed 2026-10-14. This branch builds a Windows x64 OpenVR-to-OpenXR
runtime. The current macOS target is Alyx under CrossOver Wine/DXMT, through
`macos-wine-xr`'s in-process `wineopenxr` bridge to native Monado.

```text
Alyx → Valve openvr_api.dll → xrizer bin/vrclient_x64.dll
     → Windows OpenXR loader → wineopenxr → native Monado / PS VR2
```

The DLL also exports a drop-in OpenVR API, but the current Alyx runner preserves
Valve's original `openvr_api.dll` and selects xrizer with `VR_OVERRIDE`. It places
the Windows `openxr_loader.dll` beside `hlvr.exe`. The bridge owns D3D11-to-Metal
resource adaptation; xrizer does not implement Metal or IOSurface transport.

Windows production sessions start on a temporary D3D11 device and restart on
the game's device when the first DirectX texture arrives. The ordinary submit
path copies OpenVR texture bounds into stereo OpenXR array images. The new stage
path decodes OBJ/PNG/TGA/DDS assets and draws a loading diorama; it uses a private deferred
context, cached shaders/RTVs and a bounded asynchronous asset loader. See
[the follow-up](ALYX_REVIEW_FOLLOWUP.md) for validation and remaining limits.

## Build

Install MinGW, Rust's `x86_64-pc-windows-gnu` target and `glslc`, then run:

```sh
XRIZER_VERSION=macos-dev cargo xbuild --locked --release \
  --target x86_64-pc-windows-gnu --no-default-features
```

Use the Rust toolchain that actually contains the Windows target. On this Mac,
Homebrew `rustc` and the rustup compiler have separate standard-library trees;
set `RUSTC` to the rustup toolchain's compiler when using Homebrew cargo. Bindgen
also needs the installed MinGW C++ include paths on macOS. Runtime outputs are
`target/x86_64-pc-windows-gnu/release/bin/vrclient_x64.dll` and
`target/x86_64-pc-windows-gnu/release/openvr_api.dll`.

## Evidence and remaining limits

The [Windows OpenXR workflow](https://github.com/NikNakk/xrizer/actions/runs/35930876849)
passes for committed baseline `655d448`. It cross-builds on Ubuntu, compiles the
Windows tests with `--no-run`, verifies exported symbols and uploads DLLs; it
does **not** execute Windows unit tests. The
[Linux regression workflow](https://github.com/NikNakk/xrizer/actions/runs/35930876864)
fails three input tests. The local Wine suite reproduces the same three failures.

The baseline's initial graphics restart deadlocked on a recursive session read.
The correction and subsequent cleanup are committed locally. The fixture defects
behind the three baseline input failures are repaired: local Linux tests pass
96 unit and 2 integration tests; the Windows/Wine cleanup suite passes 96 tests
with 2 ignored. Strict Windows and Linux Clippy checks pass. These local results
do not imply that a new remote CI run has passed.

Later commits through `1eb76cd` correct display-frequency fallback units, HMD
naming and successful property error reporting. User-operated Alyx gameplay with
that combined code is recorded in Monado's
`doc/macos-steamvr-keyed-ownership-followup.md`, including 45% scale at 120 Hz
and auto-fidelity behavior. Frame pacing and GPU cost remain under investigation.

The runtime now reports measured CPU phases and nonblocking D3D11 timestamp
samples, preserves timing history, rejects unsupported copy formats, bounds
image waits and returns frame-path failures rather than panicking. This does not
provide native compositor GPU telemetry or automatic session recovery. Some
inherited initialization/teardown paths still panic on errors. Temporary device
creation still uses the default adapter rather than the runtime's required LUID.
The direct launcher defaults to manual fidelity; auto-fidelity was explicitly
enabled for later hardware experiments. See the follow-up for exact limits.
