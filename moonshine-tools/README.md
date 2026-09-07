# Measuring Moonshine performance

`moonshine-bench` launches an application in Moonshine's headless compositor,
encodes its frames, sends real GameStream UDP packets, and keeps a loopback
receiver alive to verify receipt. Reports distinguish processing latency,
queueing, frame pacing, dropped work, and receiver completeness.

This measures **host processing and local UDP receipt**, not input-to-photon
latency. It does not decode video or measure a Moonlight display.

Benchmark reports, raw frame traces, and the local `baselines/` archive are
ignored by version control. Keep additional run artifacts under
`benchmark-results/` at the repository root.

## Start with a reproducible baseline

Build the benchmark and WSI layer with optimizations:

```sh
cargo build --release --workspace
```

Use the normal Moonshine runtime dependencies and matching WSI layer. On Nix,
use `nix develop -c cargo build --release --workspace`; the development shell
provides build dependencies, while runtime Vulkan/EGL drivers, XWayland,
systemd user session, and the test application must also be available.

Run three repetitions of one workload before trying a large matrix:

```sh
target/release/moonshine-bench \
  --resolution 1920x1080 --fps 60 --codec h264 \
  --warmup 5 --duration 30 --repeat 3 \
  --workload cube-1080p --label baseline --output baseline \
  -- /usr/bin/vkcube --wsi wayland --width 1920 --height 1080
```

Use an application path that exists on your system. The application dimensions
must match the stream if you want to exercise direct scanout. Check the reported
capture paths: a small cube window measures composition, not fullscreen bypass.
`--gpu` selects the compositor GPU; the encoder and application also need to use
that GPU. On systems with multiple GPUs, select the appropriate Vulkan driver
for both processes, and confirm the device in the logs.

Each `--output` directory must be new, preventing accidental replacement of a
baseline. A run creates `run-001.json`, etc. Initialization failures create
`run-001-error.json` and exit unsuccessfully. `--raw` also produces
`run-001-frames.jsonl` with timestamps, frame IDs, paths, sizes, queue depths,
send outcomes, and all host stage durations. JSONL serialization runs on a
separate thread; bounded writer overflow invalidates the run. Receiver timings
are currently aggregate distributions, not per-frame JSONL records.

**Duration is measured after warmup.** The default is 4 seconds of warmup plus
30 seconds of measurement; duration must be positive. This replaces the old
indefinite default and old duration-including-warmup behavior. A run is finite
so stalled and silent workloads finish with an explicit result.

The benchmark uses the normal `moonshine-session.service` application unit.
Run it when no streaming session is active. It does not restart the installed
Moonshine daemon. `--port` selects the benchmark's video port; the default is
47998, so choose a free port if the daemon is already bound there.

## What the measurements mean

All durations are measured with the **same host monotonic clock**. JSON durations
are in microseconds. Histograms have three significant digits and a fixed
60-second range, so their storage does not grow with frame count. Mean/min/max
use actual values; any overflow makes that distribution's percentiles `null`
instead of silently clipping its tail.

| Metric | Boundary / interpretation |
|---|---|
| `host_total` | Capture starts → all socket send attempts complete. Kernel acceptance is not NIC or client delivery. |
| `capture` | Capture starts → composed render or direct-buffer export finishes; includes scene assembly and fence wait. |
| `channel_wait` | Export completion → encoder accepts the capture. |
| `import` | CPU DMA-BUF import/cache lookup work. |
| `convert` | Color-conversion call, including GPU wait and converter setup if needed. |
| `submit` | Encoder submission call. |
| `encode_wait` | Submission returns → the packet consumer observes the encode future ready. Includes GPU encode, readback, and scheduling. |
| `packetize` | Packet becomes available → packetization completes, including HDR metadata injection and FEC. |
| `enqueue_wait` | Packetization completes → output-channel capacity is reserved. |
| `network_queue` | Reserved channel handoff → sender dequeues the frame. Includes the small publication overhead. |
| `network_send` | Sender dequeues → all socket attempts complete. |
| `loopback_first` | Capture starts → receiver observes the first shard. |
| `loopback_complete` | Capture starts → receiver observes every emitted shard, including FEC. Does not imply decodability or presentation. |
| `loopback_spread` | First → last unique received shard of a complete frame. |
| `send_interval` | Time between successful new-frame socket completions; exposes stalls/bursts independently of latency. |
| `buffer_age` | Applied attachment of the selected direct/override buffer → capture. Absent for composition/reencode. Excludes client rendering and uncommitted attachments; does not imply GPU readiness. |
| `buffer_to_send` | Per-frame `buffer_age + host_total`, summarized after adding each frame's durations. Same direct-capture scope. |
| `scene_wait` | First observed scene invalidation → capture starts. A scene-level diagnostic, not the exact age of an application's committed buffer. |
| `timer_lateness` | Capture timer callback time minus its scheduled deadline. |
| `render_wait` | Compositor CPU fence wait; **already inside `capture`**. |
| `consumer_queue` | Consumer queue/scheduling delay; **already inside `encode_wait`**. |
| `unaccounted` | Positive remainder after subtracting the additive stages from `host_total`; should be zero or very small. |

Add `capture`, `channel_wait`, `import`, `convert`, `submit`, `encode_wait`,
`packetize`, `enqueue_wait`, `network_queue`, and `network_send` to reconstruct
`host_total`. Do not add diagnostic subsets, scene wait, or loopback metrics.
CPU durations must not be interpreted as isolated GPU timestamp measurements.

Primary latency/FPS distributions contain **successful new captures** only.
Failed sends and recovery re-encodes have separate counts; host total is also
split by capture path and key/inter frame. Reported FPS uses the entire measured
window, including time without frames. Samples must start and finish inside
that window, excluding warmup-crossing frames. Read the counters alongside the
latency percentiles: missing or unfinished frames have no completed latency.

`capture_visible_update` counts accepted captures containing an applied buffer
attachment/damage update to a selected visible surface, or removal of a surface
visible in the previous accepted frame. It coalesces updates per capture and
uses the renderer's visibility decisions (including occlusion). Callback-only
commits and unselected/hidden surfaces do not count; synchronized child changes
count only after their parent applies them. Geometry, focus and cursor movement
are separate repaint reasons, so this is not a general visual-change or dropped
frame count.

Capture now runs after Wayland dispatch when an applied visible-content change
can use the current pacing opportunity. The absolute timer sends frame callbacks
independently for native windows, popups and the active WSI replacement. An unused
opportunity falls back at the next tick for late content, scene-only changes and
one-second static keepalives; opportunities do not accumulate. Each opportunity
allows one capture attempt, including a rejected export. Pending content clears
only on acceptance. Adjacent fallback and commit captures can straddle a tick,
so this caps sustained rate rather than imposing a minimum inter-frame interval.
Composition checks current clipping/occlusion before acquiring a render target,
without advancing the render damage history. Direct capture checks the selected
buffer without a GLES import. `timer_lateness` is zero for commit-triggered
captures and measures deadline lateness only for timer fallback captures.

Counters distinguish intentional static-screen skips from buffer-pool pressure,
full capture queues, pre-encode drops, import/conversion/submission/readback/FEC
failures, missing clients, and socket errors. Counter snapshots cover the wall
clock measurement interval, so work crossing a boundary prevents exact cohort
conservation. `outstanding_submitted_frames_start/end` expose unfinished work
at those boundaries. Snapshots use relaxed atomics and are approximately
simultaneous, not transactional.

The receiver deduplicates packet indices and matches the successful sender's
expected datagram count. Frames still incomplete after 250 ms are reported as
incomplete. This is a local receipt check, not Moonlight's FEC reconstruction:
a frame missing parity may still be decodable by a real client. Receiver lifetime
counters include warmup and drain; per-frame receipt distributions and incomplete
counts correspond to the measured capture samples.

The existing server log summary remains explicitly labeled
**export-to-network-enqueue**. Its boundary differs from this benchmark's
`host_total`; use the JSON results for comparisons.

## Compare a change

Repeat the same command after rebuilding, using `--label candidate --output
candidate`, then:

```sh
python3 moonshine-tools/compare.py baseline candidate
```

The comparison reports the median and range of **per-run** p50/p95/p99 values,
throughput, missed frame budgets, queue pressure, receipt completeness, and
errors. It does not average percentiles into a pooled percentile or claim
statistical significance. It rejects invalid runs and mismatched workload
settings, hardware, driver, or kernel metadata. `--allow-environment-change`
is available for intentional hardware/driver comparisons.

For a descriptive regression gate:

```sh
python3 moonshine-tools/compare.py baseline candidate --fail-percent 5
```

Exit status is 0 for no gated regression, 1 for invalid/incompatible data, and 2
for a regression above the threshold. New nonzero failures/drops are regressions
from a zero baseline. Percent changes near zero can be noisy; evaluate absolute
values and run ranges as well.

A reasonable optimization experiment uses at least three, preferably five,
repetitions. Keep the scene, application arguments, graphics settings, frame
cap, codec, bitrate, FEC, resolution, HDR, and logging identical. Alternate the
baseline and candidate binaries when checking smaller differences to reduce
thermal and background-load drift. The report fingerprints the executable
with SHA-256 where available; the recorded working-tree revision describes
source state **at run time**, not proof of what an older binary contains.

Use `--raw` to investigate spikes after a normal run reveals them; compare
both builds with the same raw/logging settings. Keep periodic INFO logs for
normal runs. `--verbose` and debug/trace logging can perturb scheduling.
Debug builds, interrupted/short runs, missing successful frames, lost stats,
raw-writer overflow, and receiver errors fail measurement-validity checks.
Workload drops and socket failures remain measured outcomes rather than being
filtered away as invalid experiments.

## Exercise the relevant paths

- **Direct scanout:** one fullscreen DMA-BUF surface, no cursor or popup overlay;
  confirm `Direct`/`DirectOverride` samples.
- **Composition:** add `--composited` to force the GLES path without changing
  application content. This is a different benchmark configuration, so inspect
  its reports separately; ordinary before/after comparisons must match this flag.
- **CPU/network packetization:** increase bitrate or use larger keyframes. Keep
  FEC fixed while comparing implementations, then run separate FEC experiments.
- **GPU pressure:** use a deterministic in-game replay at representative settings;
  the cube validates the pipeline but does not reproduce a saturated game GPU.
- **Cadence:** compare 60 and 120 FPS, then higher rates supported by the workload.
  Inspect successful FPS, send intervals, timer lateness, and drop counters.
- **HDR and codec coverage:** use actual HDR content and supported GPU codecs.
  A negotiated HDR session with an SDR cube is not an HDR-game benchmark.

The full built-in matrix is still available:

```sh
target/release/moonshine-bench --matrix --duration 30 --warmup 5 \
  --output matrix -- /path/to/your/size-aware-test-application
```

It covers 1080p/1440p/4K × 60/120/360 FPS × H.264/HEVC/AV1. The application can
read `MOONSHINE_CLIENT_WIDTH`, `MOONSHINE_CLIENT_HEIGHT`, and
`MOONSHINE_CLIENT_FRAMERATE` for sizing and pacing. Unsupported codecs fail explicitly, the matrix continues, and the final exit
status is unsuccessful if any run failed. Select supported
single configurations when establishing a hardware baseline.

## Beyond the host

Loopback does not exercise the physical NIC, Wi-Fi/WAN, client decode queues,
or display scheduling. To optimize perceived latency, pair host measurements
with a repeatable Moonlight workload and client frame/drop/decode statistics.
For input-to-photon validation, use a timestamped visual response to injected
input and a high-speed camera or synchronized capture setup. Keep that result
separate from `host_total`; do not subtract unsynchronized client and host clocks.

## Validation

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets -- -D warnings
cargo fmt --all -- --check
python3 -m unittest discover -s moonshine-tools -p 'test_*.py'
```

Tests cover additive timing boundaries, failure accounting, percentile overflow,
re-encode filtering, complete/duplicate UDP receipt, and comparison safeguards.
The UDP integration test uses a real local socket and needs no GPU. GPU-backed
runs remain necessary to validate actual compositor/driver/encoder behavior.
