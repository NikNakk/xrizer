# Alyx review follow-up — 2026-10-14

The cleanup of the Windows/Alyx changes is committed locally on
`windows-openxr-d3d11`. The initial findings are preserved in
[the review](ALYX_CHANGE_REVIEW.md); this document states their current disposition.

## Retained and corrected

- `40099f0`: commit decoder dependencies to Cargo.lock. Windows CI now uses
  `--locked` for builds and test compilation.
- `6bc87ff`: initialize API cores safely and retain stable version-specific
  client factories. Keep Valve-compatible true/required-size semantics for
  `VR_GetRuntimePath` when the output buffer is short.
- `c88ab64`: reuse the existing session read guard in digital/analog binding
  queries, preventing first-submit restart from deadlocking behind a writer.
- `68aede4`: synchronize mailbox readiness and retain pending messages across
  sizing/short-buffer queries; log payloads at trace level.
- `86ccd5c`: remove fake controllers and diagnostic input switches, and repair
  the missing secondary-action/pose bindings and origin-test synchronization.
- `3fffac0`: isolate stage drawing with a deferred context and restored app
  state, cache per-eye RTVs and compile shader bytecode once per process.
  Bind stage constants after deferred-context upload so DXMT captures the current
  backing allocation. Add bounded nonblocking timestamp/disjoint query storage.
- `cb2986a`: implement measured CPU phases and GPU-completed timing history,
  bound image waits, latch runtime frame failures, validate D3D11 color formats,
  and load stage assets asynchronously with allocation limits and generation-based
  cancellation. Retain the temporary session's frame controller for loading.

CPU staging copies, source/test clears, flush experiments, head-relative fake
hands and ignore-stage/suspend/skybox/projection switches have been removed.
Ordinary D3D11 copying, stage decoding/rendering, API compatibility fixes and
Windows platform/export support remain. Environment options left in the runtime
are setup options rather than per-frame behavioral experiments. The launcher
no longer exports the removed prefer-application-projection switch.

Claude subsequently committed `15e9926`, `c6ff3f9` and `1eb76cd`: report display
frequency as a rate, name the HMD from OpenXR, and write Success for recognized
float properties. These changes are preserved alongside the cleanup.

## Validation and limits

At the cleanup commits, local Windows/Wine tests pass **96, with 2 ignored**;
the session-lock regression passes ten repetitions. Local Linux tests pass
**96 unit and 2 integration tests**. Strict Clippy passes for Windows and Linux,
and formatting/whitespace checks pass. A real Wine/DXMT graphics fixture verifies
app state preservation and distinct left/right stage pixels, and the actual DLL
export loader verifies versioned factories and short-buffer semantics.

At combined commit `1eb76cd`, a fresh Windows/Wine test build passes **97,
with 2 ignored**, and the deadlock regression still passes ten repetitions.
The installed `~/Windows/openvr-runtimes/xrizer/bin/vrclient_x64.dll` matches
that release build (SHA-256
`f82c751f0bce49952298380df472f34ceca7480dddec712c4c3d484803fd87d2`).

An ARM64 Miri run stops on an unsupported LLVM NEON intrinsic; the attempted x64
Miri run lacks cross-target bindgen headers. Full Miri success is not claimed.
Windows builds emit a host rust-objcopy/libLLVM stripping warning; the produced
Windows test executables run. These are local checks, not a new remote CI result.

Later user-operated gameplay at combined xrizer `1eb76cd` is recorded in Monado's
`doc/macos-steamvr-keyed-ownership-followup.md`. Startup now proceeds into play.
The latest evidence puts long frame gaps inside the application frame, consistent
with DXMT backpressure; it does not establish a resolved pacing problem.

D3D11 timing measures the query span containing application/copy work, not pure
GPU utilization or Monado compositor GPU cost. Pending/disjoint samples are not
reported as zero-cost completed frames. The launch script still defaults to
manual fidelity; later hardware experiments explicitly enabled auto-fidelity.

Frame failures stop image reuse and return RequestFailed; they do not recreate
lost sessions. Inherited initialization/teardown panic paths remain. Temporary
D3D11 adapter selection still needs required-LUID handling. Stage cancellation
suppresses stale publication but does not forcibly interrupt an ongoing decode;
shutdown joins its worker. MSAA/source-array copying, arbitrary color conversion
and nonzero stage Fresnel effects return unsupported results. Further hardware
validation remains user-operated.
