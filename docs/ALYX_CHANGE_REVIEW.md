# Critical review of the Windows/Alyx changes — 2026-10-14

Reviewed `windows-openxr-d3d11` baseline `655d448`, its Windows/Alyx commit series
back to upstream base `0989a7f`, and the local startup-deadlock correction.
This document preserves the initial review findings, line references and test
results at that baseline; they are historical, not the current outstanding-work
list. The subsequent cleanup is committed and described in
[the follow-up](ALYX_REVIEW_FOLLOWUP.md). Claude's later property fixes and the
user's successful gameplay evidence supersede the initial startup-only status.

## Findings by priority

### P1: stage rendering overwrites the application's graphics state

`src/graphics_backends/d3d11_stage.rs:315` binds IA/VS/PS/RS/OM state on the
same immediate context obtained from the game's D3D11 device. At line 390 it
calls `ClearState`, which resets state rather than restoring the application's
previous state. An error creating either eye's RTV also returns after modifying
state, before any cleanup. Application engines cache their bindings, so a later
game draw can reuse assumptions that no longer match the actual context.

Do not promote this renderer to a validated production feature. Use a private
deferred context and execute with restore-state semantics, or a complete scoped
state save/restore on every path. Check context-thread synchronization too; the
backend does not arrange its own multithread protection. No visual regression
from this path has been established by the existing capture.

### P1: runtime errors still panic across OpenVR calls

`src/compositor.rs:1452` and `1494` call `release_image().unwrap()`. Begin-frame
has another unconditional release unwrap; acquisition, infinite image wait,
`xrWaitFrame`, `xrBeginFrame`, swapchain creation and `xrEndFrame` also panic on
runtime failure. The issue is wider than one unwrap and partly inherited.

A runtime/session-loss result can abort the game at the FFI boundary. Handle it
as a lifecycle transition: propagate a compositor failure from Submit, stop
reusing/presenting uncertain images, retire the session, and bound waits. Merely
logging an unsuccessful release and marking the image reusable would be wrong.
This requires fault-injected lifecycle tests; it was not implemented in this review.

### P1: existing Linux CI is red

[Windows cross-build passes](https://github.com/NikNakk/xrizer/actions/runs/35930876849)
for `655d448`, but its Windows tests are compiled, not run.
[Linux regression fails](https://github.com/NikNakk/xrizer/actions/runs/35930876864)
in three input tests. These same failures reproduce under Wine, before and after
the startup fix/review changes. They cannot be dismissed as Wine-only issues.
Two secondary-set tests request `/actions/set2/in/boolact`, which is absent from
the fixture's actions list despite the set being declared. The origin test
returns no left pose origin. Repair/validate the fixtures and semantics separately;
these failures do not establish working secondary action sets or controller origins.

### P2: timing is synthetic and historical queries are unimplemented

`src/compositor.rs:711` returns constant 8/1/9 ms application GPU durations and
fixed compositor durations. `_frames_ago` is ignored, index/time are independent
atomics, and some caller fields such as HMD pose/drop counts remain untouched.
`GetFrameTimings` is a `todo!()` panic. These are inherited from `0989a7f`, not
introduced by the Windows port, but inherited does not mean acceptable.

Keep OpenVR timing API support, replace the fabricated success data. CPU wall
clocks around Submit do not measure the application's GPU render duration, and
OpenXR predicted display period is not GPU time. For D3D11, use asynchronous
GPU timestamp/disjoint queries spanning application work, a bounded completed
frame history, actual CPU phase timing and explicit unavailable/disjoint samples.
Compositor GPU timing may require bridge/runtime telemetry. Then validate Alyx's
fidelity response on hardware; real timestamps are necessary evidence, not a
promise that auto-fidelity will automatically work. The direct launcher currently
passes `+vr_fidelity_level_auto 0`, independently disabling auto-fidelity.

### P2: initial loading-stage support has a lifecycle hole

`SessionData::new` leaves `comp_data` empty, and the first waiter/stream are not
installed into a frame controller. `maybe_wait_frame`, `maybe_begin_frame` and
handoff have no renderer until a real Submit restarts the session. In the captured
Alyx run the startup stage is loaded, loading ends, the stage is cleared, and
only then does the first Submit create the real backend. Thus that capture does
not show any startup-stage pixels, although a ready event was sent.

Stage support should remain an isolated experimental feature until the temporary
session can actually pace/render loading frames and transition safely to the game.
A compatibility no-op can be justified only by fresh behavioral evidence; it
must not be described as a complete stage implementation.

### P2: D3D11 ignores color-space and compatible-format constraints

`src/graphics_backends/d3d11.rs:270` ignores the OpenVR color-space argument and
selects `desc.Format`. Auto/Gamma/Linear submissions therefore do not select
appropriate sRGB/linear runtime formats. The backend also preserves source MSAA
sample count, assumes array source subresource 0 and raw-copy compatibility,
while the in-process bridge accepts single-sample images and a small color-format
set. Runtime format fallback can select an incompatible destination for a raw
copy. Retain ordinary D3D11 support, but validate source/destination compatibility,
choose supported formats deliberately and explicitly reject unsupported cases
until resolve/conversion support is validated.

### P2: synchronous stage loading has concurrency and memory costs

`SetStageOverride_Async` at `src/compositor.rs:355` does file IO/OBJ/image decoding
synchronously. It checks for an existing stage, releases the mutex, loads, then
sets it: simultaneous callers can overwrite each other, and ClearStageOverride
can race with an in-flight load. An 8192² decoded RGBA texture alone is 256 MiB.
Use a bounded cancellable worker with a generation/token and readiness only when
usable; reject excessive asset allocations. This is more than a misleading name.

### P2: mailbox readiness can be lost on a sizing query

`src/misc_unknown.rs:139` checks/stores a global AtomicBool before checking output
capacity. A short-buffer call consumes the only ready message, and two concurrent
readers can both observe false. Null output pointers are not checked before copy.
These defects predate the recent ready-payload length change. Keep the observed
Alyx/OpenComposite payload semantics, but represent a pending message with a
synchronized mailbox state, consume only after successful copy, and test retries
and register/unregister lifetimes. Mailbox support itself is not full web UI support.

## Disposition of the requested items

| Item | Decision |
| --- | --- |
| Cargo.lock decoder dependencies | Keep and commit. Done as `40099f0` (+286/-30); locked offline metadata succeeds. Windows CI source now uses `--locked`. |
| Fake frame timing | Replace, not remove the API. Dedicated GPU/history work remains outstanding. |
| openvr_api_core check/set | Fixed to `get_or_init`. Old OnceLock publication was memory-safe but could construct discarded cores. |
| VRClientCoreFactory check/set | Stronger inherited bug: concurrent `set` panics and one cached pointer ignores later requested versions. Fixed with separate stable versioned cores and no leaked racing candidates. |
| VR_GetRuntimePath short buffer | Keep true. [Valve's implementation](https://github.com/ValveSoftware/openvr/blob/master/src/openvr_api_public.cpp#L267) also returns true and reports required size; a regression now protects this behavior. The executable-directory path itself is only a drop-in approximation of runtime installation. |
| release_image unwrap | Correct finding; replace through a consistent failure-state design, not isolated ignored errors. |
| Per-frame environment reads | Fixed: immutable process configuration reads flags once; setup-only custom-binding/tracker-string options remain at setup. |
| Disabled input diagnostics | Fixed eager marker/path allocations and moved mailbox payloads to trace. Passive diagnostics are not automatically failed experiments, but a separate Alyx flag can eventually become ordinary trace logging. |
| CPU-copy, source/test clears, pre/post flush | Remove from the runtime or move to a standalone graphics test harness after retaining evidence. None is a demonstrated fix for the confirmed session-lock deadlock. They remain in source here so review does not silently discard past experiment capability. |
| Fake controllers | Remove from ordinary runtime policy; use fake runtime/test fixtures. They invent tracked head-relative hands and active neutral actions and can hide broken real-controller bindings. |
| Ignore stage/suspend, hide skybox, prefer application projection | Retire behavioral experiment switches unless a fresh regression/visual test demonstrates a lasting compatibility need. In particular, the launcher enables prefer-projection, so removing it requires a deliberate loading-scene comparison. |
| Async stage API | Keep the API, redesign the implementation and cancellation/readiness semantics. |
| Runtime shader compilation | Two shaders compile once per new stage renderer, not once per frame. Cache pipeline objects per device or package precompiled shaders; this is a load-stall/dependency issue. |
| Per-frame stage RTVs | Cache per destination image/eye, invalidate at swapchain/device replacement. Correctness/state restoration comes first. |
| DDS/image support | Retain if real stage rendering is retained: captured Alyx assets are BC4 DDS. General decoder dependency size alone is not proof it should be removed. Move the entire chain out with stage support if stage rendering is deferred. |
| Windows ABI/platform/adapter/export fixes | Retain. They are the foundation of the supported Windows D3D11 route. Default-adapter selection still needs required-LUID handling. |
| Controller enumerations, property strings, dominant hand, zero-size origins, secondary sets | Retain actual API/compatibility fixes, independent of fake-controller switches; fix the red input tests before claiming validation. |
| Design document | Updated to actual wineopenxr architecture, Valve-loader launch, passing cross-build, failed Linux tests and hardware limits. |

## Validation of review edits

Windows cross-compilation succeeds. Wine runs 89 unit tests: **84 pass, 3 fail,
2 ignored**. The two new factory/path tests pass, and the startup deadlock
regression passes ten repetitions. The three failures match the previous
baseline. Builds emit an existing host rust-objcopy/libLLVM debug-strip warning;
the compiled Windows executables run successfully. No new CI run or headset
acceptance is claimed. Source whitespace checks pass.

The recommended pruning, GPU timing, stage redesign and runtime-fault recovery
are review findings, not represented as completed implementations. The local
review changes do not alter the installed headset runtime DLL.
