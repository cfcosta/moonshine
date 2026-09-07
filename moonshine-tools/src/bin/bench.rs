use std::time::{Duration, Instant};

use async_shutdown::ShutdownManager;
use clap::Parser;
use moonshine_core::ShutdownReason;
use moonshine_core::config::ApplicationConfig;
use moonshine_core::session::SessionContext;
use moonshine_core::session::SessionKeyData;
use moonshine_core::session::SessionKeys;
use moonshine_core::session::compositor::CompositorConfig;
use moonshine_core::session::manager::SessionManager;
use moonshine_core::session::stream::audio::AudioChannels;
use moonshine_core::session::stream::audio::AudioConfig;
use moonshine_core::session::stream::audio::AudioStreamConfig;
use moonshine_core::session::stream::audio::AudioStreamContext;
use moonshine_core::session::stream::control::ControlStreamConfig;
use moonshine_core::session::stream::video::FrameStats;
use moonshine_core::session::stream::video::VideoChromaSampling;
use moonshine_core::session::stream::video::VideoDynamicRange;
use moonshine_core::session::stream::video::VideoFormat;
use moonshine_core::session::stream::video::VideoStreamConfig;
use moonshine_core::session::stream::video::VideoStreamContext;
use tokio::signal;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "bench/raw.rs"]
mod raw;
#[path = "bench/receiver.rs"]
mod receiver;
#[path = "bench/report.rs"]
mod report;

#[derive(Parser, Debug)]
#[command(
	name = "moonshine-bench",
	about = "Measure host video latency and loopback receipt (not client display latency)"
)]
struct Args {
	/// Application and arguments. Use -- before the command.
	#[arg(required = true, trailing_var_arg = true)]
	command: Vec<String>,
	/// Built-in 4K/1440p/1080p x 60/120/360 FPS x HEVC/H.264/AV1 matrix.
	#[arg(long)]
	matrix: bool,
	#[arg(long, default_value = "1920x1080")]
	resolution: String,
	#[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=1000))]
	fps: u32,
	#[arg(long, default_value_t = 20_000_000)]
	bitrate: usize,
	#[arg(long, default_value = "h264", value_parser = ["h264", "hevc", "av1"])]
	codec: String,
	/// Measurement seconds AFTER warmup; finite runs make comparisons repeatable.
	#[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..))]
	duration: u64,
	#[arg(long, default_value_t = 4)]
	warmup: u64,
	#[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
	repeat: u32,
	#[arg(long)]
	hdr: bool,
	#[arg(long)]
	gpu: Option<String>,
	/// Force the GLES compositor path for a controlled comparison.
	#[arg(long)]
	composited: bool,
	#[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u8).range(0..=100))]
	fec: u8,
	#[arg(long, default_value_t = 47998, value_parser = clap::value_parser!(u16).range(1..))]
	port: u16,
	/// New directory for JSON reports. Existing directories are rejected.
	#[arg(long)]
	output: Option<PathBuf>,
	/// Include per-frame JSONL using a separate bounded writer thread.
	#[arg(long, requires = "output")]
	raw: bool,
	/// Human workload description, e.g. game/scene/settings, included in comparisons.
	#[arg(long, default_value = "unspecified")]
	workload: String,
	/// Label the build/experiment without changing workload compatibility.
	#[arg(long, default_value = "unlabelled")]
	label: String,
	/// Log each measured frame; use only for diagnosis (can perturb timings).
	#[arg(long)]
	verbose: bool,
}

type Error = Box<dyn std::error::Error>;
fn error(message: impl Into<String>) -> Error {
	std::io::Error::other(message.into()).into()
}
fn parse_resolution(text: &str) -> Result<(u32, u32), Error> {
	let (w, h) = text.split_once('x').ok_or_else(|| error("resolution must be WxH"))?;
	let (w, h) = (w.parse::<u32>()?, h.parse::<u32>()?);
	if w == 0 || h == 0 {
		return Err(error("resolution must be positive"));
	}
	Ok((w, h))
}
fn parse_codec(codec: &str) -> VideoFormat {
	match codec {
		"h264" => VideoFormat::H264,
		"hevc" => VideoFormat::Hevc,
		"av1" => VideoFormat::Av1,
		_ => unreachable!(),
	}
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
	let result = std::process::Command::new(program).args(args).output().ok()?;
	result
		.status
		.success()
		.then(|| String::from_utf8_lossy(&result.stdout).trim().to_string())
}
fn environment() -> serde_json::Value {
	let gpus: BTreeMap<_, _> = std::fs::read_dir("/sys/class/drm")
		.into_iter()
		.flatten()
		.flatten()
		.filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
		.map(|entry| {
			(
				entry.file_name().to_string_lossy().to_string(),
				std::fs::read_to_string(entry.path().join("device/uevent")).unwrap_or_default(),
			)
		})
		.collect();
	let executable = std::env::current_exe().ok();
	json!({
		"version": env!("CARGO_PKG_VERSION"), "debug_assertions": cfg!(debug_assertions),
		"executable": executable,
		"binary_sha256": executable.as_ref().and_then(|p| command_output("sha256sum", &[p.to_str()?]))
			.and_then(|s| s.split_whitespace().next().map(str::to_string)),
		"working_tree_revision_at_run": command_output("git", &["rev-parse", "HEAD"]),
		"working_tree_changes_at_run": command_output("git", &["status", "--porcelain"]),
		"kernel": std::fs::read_to_string("/proc/sys/kernel/osrelease").ok(),
		"cpu": std::fs::read_to_string("/proc/cpuinfo").ok().and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(str::to_string)),
		"available_render_devices": gpus,
		"nvidia_driver": std::fs::read_to_string("/proc/driver/nvidia/version").ok(),
		"runtime_gpu_settings": (["VK_DRIVER_FILES", "VK_ICD_FILENAMES", "VK_LAYER_PATH", "VK_INSTANCE_LAYERS", "__GLX_VENDOR_LIBRARY_NAME", "MOONSHINE_LOG", "MOONSHINE_RENDER_NODE", "LD_LIBRARY_PATH"]
			.into_iter().map(|key| (key, std::env::var(key).ok())).collect::<BTreeMap<_,_>>()),
	})
}

#[tokio::main]
async fn main() -> Result<(), Error> {
	tracing_subscriber::registry()
		.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
		.with(EnvFilter::try_from_env("MOONSHINE_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
		.init();
	let args = Args::parse();
	parse_resolution(&args.resolution)?;
	if args.bitrate == 0 || args.bitrate > u32::MAX as usize {
		return Err(error("bitrate must fit a positive u32"));
	}
	if args
		.duration
		.checked_add(args.warmup)
		.is_none_or(|seconds| seconds > 86400)
	{
		return Err(error("duration plus warmup must be at most 86400 seconds"));
	}
	if let Some(output) = &args.output {
		std::fs::create_dir(output)?;
	}
	let environment = environment();
	if cfg!(debug_assertions) {
		tracing::warn!("Debug build: results will be marked unsuitable for performance comparisons; use --release");
	}
	let settings: Vec<(String, u32, String)> = if args.matrix {
		["3840x2160", "2560x1440", "1920x1080"]
			.into_iter()
			.flat_map(|resolution| {
				[60, 120, 360].into_iter().flat_map(move |fps| {
					["hevc", "h264", "av1"]
						.into_iter()
						.map(move |codec| (resolution.into(), fps, codec.into()))
				})
			})
			.collect()
	} else {
		vec![(args.resolution.clone(), args.fps, args.codec.clone())]
	};
	let mut invalid = 0;
	let mut run = 0;
	for repetition in 1..=args.repeat {
		for (resolution, fps, codec) in &settings {
			run += 1;
			let result = run_benchmark(
				&args,
				resolution,
				*fps,
				codec,
				args.duration,
				run,
				repetition,
				&environment,
			)
			.await;
			match result {
				Ok((valid, interrupted)) => {
					invalid += usize::from(!valid);
					if interrupted {
						return Err(error("benchmark interrupted; partial report retained"));
					}
				},
				Err(err) => {
					if let Some(output) = &args.output {
						serde_json::to_writer_pretty(
							File::create_new(output.join(format!("run-{run:03}-error.json")))?,
							&json!({"schema_version":1,"valid":false,"error":err.to_string(),
                                "resolution":resolution,"fps":fps,"codec":codec,"repetition":repetition}),
						)?;
					}
					if !args.matrix {
						return Err(err);
					}
					tracing::error!("Matrix run {run} failed: {err}");
					invalid += 1;
				},
			}
		}
	}
	if invalid > 0 {
		return Err(error(format!(
			"{invalid} run(s) failed measurement validity checks; see reports"
		)));
	}
	Ok(())
}

fn resolve_receipts(
	pending: &mut VecDeque<FrameStats>,
	receiver: &receiver::Receiver,
	accumulator: &mut report::Accumulator,
	expire: bool,
) {
	let mut remaining = VecDeque::new();
	while let Some(frame) = pending.pop_front() {
		if let Some(receipt) = receiver.take_complete(frame.frame_number, frame.sent_datagrams) {
			accumulator.record(
				"loopback_first",
				report::us(receipt.first.saturating_duration_since(frame.capture_started_at)),
			);
			accumulator.record(
				"loopback_complete",
				report::us(receipt.last.saturating_duration_since(frame.capture_started_at)),
			);
			accumulator.record(
				"loopback_spread",
				report::us(receipt.last.duration_since(receipt.first)),
			);
		} else if expire || frame.completed_at.elapsed() >= Duration::from_millis(250) {
			accumulator.incomplete_receipts += 1;
			receiver.discard(frame.frame_number);
		} else {
			remaining.push_back(frame);
		}
	}
	*pending = remaining;
}

#[allow(clippy::too_many_arguments)]
async fn run_benchmark(
	args: &Args,
	resolution: &str,
	target_fps: u32,
	codec: &str,
	duration: u64,
	run: usize,
	repetition: u32,
	environment: &serde_json::Value,
) -> Result<(bool, bool), Error> {
	let (width, height) = parse_resolution(resolution)?;
	let video_format = parse_codec(codec);
	tracing::info!(
		"Run {run}: {resolution} {target_fps} FPS {codec}; {}s warmup + {duration}s measurement",
		args.warmup
	);
	let shutdown = ShutdownManager::<ShutdownReason>::new();
	let session_manager = SessionManager::new(
		CompositorConfig {
			gpu: args.gpu.clone(),
			direct_scanout: !args.composited,
			..Default::default()
		},
		VideoStreamConfig {
			port: args.port,
			fec_percentage: args.fec,
			..Default::default()
		},
		AudioStreamConfig { port: 0 },
		ControlStreamConfig {
			port: 0,
			..Default::default()
		},
		"127.0.0.1".to_string(),
		duration.saturating_add(args.warmup).saturating_add(120),
		false,
		shutdown.clone(),
	)
	.map_err(|_| error("Failed to create session manager"))?;

	let mut stats_rx = session_manager.bench_stats_receiver();

	let app_config = ApplicationConfig {
		title: "bench".to_string(),
		stdout: Some("journal".to_string()),
		stderr: Some("journal".to_string()),
		command: args.command.clone(),
		..Default::default()
	};

	let session_ctx = SessionContext {
		application: app_config,
		application_id: 1,
		resolution: (width, height),
		refresh_rate: target_fps,
		keys: SessionKeys::Keys(SessionKeyData {
			remote_input_key: vec![0u8; 16],
			remote_input_key_id: 0,
		}),
		audio_channels: AudioChannels::Stereo,
		audio_channel_mask: 0x3,
		hdr: args.hdr,
	};

	tracing::info!("Initializing session...");
	session_manager
		.initialize_session(session_ctx)
		.await
		.map_err(|_| error("Failed to initialize session"))?;

	tracing::info!("Launching session (compositor + app)...");
	if let Err(err) = session_manager.launch_session().await {
		let _ = session_manager.stop_session().await;
		return Err(error(format!("Failed to launch session: {err:?}")));
	}

	let video_ctx = VideoStreamContext {
		width,
		height,
		fps: target_fps,
		packet_size: 1400,
		bitrate: args.bitrate,
		minimum_fec_packets: 2,
		qos: false,
		video_format,
		dynamic_range: if args.hdr {
			VideoDynamicRange::Hdr
		} else {
			VideoDynamicRange::Sdr
		},
		chroma_sampling_type: VideoChromaSampling::Yuv420,
		max_reference_frames: 1,
		full_range: false,
		encrypt_video: false,
	};

	let audio_ctx = AudioStreamContext {
		packet_duration_ms: 20,
		qos: false,
		audio_config: AudioConfig::default(),
		encrypt_audio: false,
	};

	tracing::info!("Setting stream contexts...");
	if let Err(err) = session_manager.set_stream_context(video_ctx, audio_ctx).await {
		let _ = session_manager.stop_session().await;
		return Err(error(format!("Failed to set stream context: {err:?}")));
	}

	tracing::info!("Starting session streams...");
	if let Err(err) = session_manager.start_session().await {
		let _ = session_manager.stop_session().await;
		return Err(error(format!("Failed to start session: {err:?}")));
	}

	// Keep the receiver alive for the whole session, including teardown. Bind
	// errors are fatal: a run without a receiver is not a network benchmark.
	let receiver = match receiver::Receiver::start(format!("127.0.0.1:{}", args.port).parse()?) {
		Ok(receiver) => receiver,
		Err(err) => {
			let _ = session_manager.stop_session().await;
			return Err(err.into());
		},
	};
	session_manager.trigger_streams_start().await;
	let origin = Instant::now();
	let warmup_deadline = origin + Duration::from_secs(args.warmup);
	let deadline = warmup_deadline + Duration::from_secs(duration);
	let mut writer = match &args.output {
		Some(output) if args.raw => {
			match raw::RawWriter::new(&output.join(format!("run-{run:03}-frames.jsonl")), origin) {
				Ok(writer) => Some(writer),
				Err(err) => {
					let _ = session_manager.stop_session().await;
					return Err(err.into());
				},
			}
		},
		_ => None,
	};
	let mut accumulator = report::Accumulator::default();
	let mut pending = VecDeque::new();
	let mut counter_start = None;
	let mut measurement_start = warmup_deadline;
	let mut stats_lost = 0u64;
	let mut interrupted = false;
	let mut tick = tokio::time::interval(Duration::from_millis(20));
	tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
	let mut last_print = Instant::now();
	loop {
		// Deadline branches precede frames so a flooded subscriber cannot extend a run.
		tokio::select! {
			biased;
			_ = signal::ctrl_c() => { interrupted = true; break; },
			_ = tokio::time::sleep_until(deadline.into()) => break,
			_ = tokio::time::sleep_until(warmup_deadline.into()), if counter_start.is_none() => {
				measurement_start = Instant::now();
				counter_start = Some(session_manager.bench_counters());
				tracing::info!("Warmup complete; recording measurement window");
			},
			_ = tick.tick() => {
				resolve_receipts(&mut pending, &receiver, &mut accumulator, false);
				if last_print.elapsed() >= Duration::from_secs(5) && counter_start.is_some() {
					tracing::info!("Measured {} successful frames, {} failed; {} stats messages lost",
						accumulator.successful_frames, accumulator.failed_frames, stats_lost);
					last_print = Instant::now();
				}
			},
			result = stats_rx.recv() => match result {
				Ok(stats) if counter_start.is_some() && stats.capture_started_at >= measurement_start && stats.completed_at < deadline => {
					if args.verbose { tracing::info!(frame=stats.frame_number, host_us=report::us(stats.total), path=?stats.capture_path, "frame"); }
					accumulator.add(&stats, target_fps);
					if let Some(writer) = &mut writer { writer.record(&stats); }
					if stats.send_success && stats.capture_path != moonshine_core::session::stream::video::CapturePath::Reencode {
						pending.push_back(stats);
					} else { receiver.discard(stats.frame_number); }
				},
				Ok(stats) => receiver.discard(stats.frame_number),
				Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
					if counter_start.is_some() { stats_lost += n; }
				},
				Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
			},
		}
	}
	let ended_at = Instant::now().min(deadline);
	let counter_end = session_manager.bench_counters();
	// Drain telemetry already published before the deadline (without measuring
	// teardown), otherwise the last few frames would depend on subscriber wakeup.
	loop {
		let stats = match stats_rx.try_recv() {
			Ok(stats) => stats,
			Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
				stats_lost += n;
				continue;
			},
			Err(_) => break,
		};
		if counter_start.is_some() && stats.capture_started_at >= measurement_start && stats.completed_at < ended_at {
			accumulator.add(&stats, target_fps);
			if let Some(writer) = &mut writer {
				writer.record(&stats);
			}
			if stats.send_success && stats.capture_path != moonshine_core::session::stream::video::CapturePath::Reencode
			{
				pending.push_back(stats);
			}
		}
	}
	// Receipt grace is outside the measured window. It does not inflate FPS.
	let receipt_deadline = Instant::now() + Duration::from_millis(250);
	while !pending.is_empty() && Instant::now() < receipt_deadline {
		resolve_receipts(&mut pending, &receiver, &mut accumulator, false);
		if !pending.is_empty() {
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
	}
	resolve_receipts(&mut pending, &receiver, &mut accumulator, true);
	let stop_result = session_manager.stop_session().await;
	let raw_dropped = match writer {
		Some(writer) => writer.finish()?,
		None => 0,
	};
	let elapsed = ended_at.saturating_duration_since(measurement_start);
	let summary = accumulator.summary(elapsed);
	let counter_start = counter_start.unwrap_or_else(|| counter_end.clone());
	let outstanding_start = report::outstanding(&counter_start);
	let outstanding_end = report::outstanding(&counter_end);
	let counters = report::counter_delta(&counter_end, &counter_start);
	let mut invalid_reasons = Vec::new();
	if cfg!(debug_assertions) {
		invalid_reasons.push("debug_build");
	}
	if interrupted {
		invalid_reasons.push("interrupted");
	}
	if elapsed < Duration::from_secs(duration).saturating_sub(Duration::from_millis(100)) {
		invalid_reasons.push("short_measurement_window");
	}
	if summary.successful_frames == 0 {
		invalid_reasons.push("no_successful_frames");
	}
	if stats_lost > 0 {
		invalid_reasons.push("stats_messages_lost");
	}
	if raw_dropped > 0 {
		invalid_reasons.push("raw_records_lost");
	}
	if receiver.counters().errors > 0 {
		invalid_reasons.push("receiver_error");
	}
	if stop_result.is_err() {
		invalid_reasons.push("session_stop_failed");
	}
	summary.print();
	tracing::info!(?counters, stats_lost, raw_dropped, ?invalid_reasons, "Run quality");
	let valid = invalid_reasons.is_empty();
	if let Some(output) = &args.output {
		let document = json!({
			"schema_version": 1,
			"label": args.label, "run": run, "repetition": repetition,
			"unix_time_seconds": SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
			"config": {"resolution":resolution,"fps":target_fps,"codec":codec,
				"bitrate":args.bitrate,"composited":args.composited,"fec_percentage":args.fec,"minimum_fec_packets":2,
				"packet_size":1400,"hdr":args.hdr,"gpu":args.gpu,"chroma":"yuv420",
				"reference_frames":1,"encrypt_video":false,"full_range":false,
				"duration_seconds":duration,"warmup_seconds":args.warmup,
				"command":args.command,"workload":args.workload,"verbose":args.verbose,"raw":args.raw},
			"environment":environment,
			"valid":valid,"invalid_reasons":invalid_reasons,
			"stats_messages_lost":stats_lost,"raw_records_lost":raw_dropped,
			"counters":counters,"outstanding_submitted_frames_start":outstanding_start,
			"outstanding_submitted_frames_end":outstanding_end,"receiver_lifetime_counters":receiver.counters(),
			"summary":summary,
			"measurement": {
				"host_total":"capture start to completion of all socket send attempts; successful new captures only",
				"loopback_complete":"capture start to userspace receipt of every emitted shard, including FEC; no decode",
				"scene_wait":"first observed scene invalidation to capture start; diagnostic, not per-buffer presentation age",
				"diagnostic_subsets":["render_wait within capture","consumer_queue within encode_wait"],
				"histogram":"microseconds, 3 significant digits, fixed 60-second range; overflow invalidates percentiles",
				"counter_window":"wall-clock measurement interval; boundary frames can be in flight, so counters are not a cohort identity",
				"excluded":["input-to-game latency","application render time","client decode","client display"]
			}
		});
		serde_json::to_writer_pretty(File::create_new(output.join(format!("run-{run:03}.json")))?, &document)?;
	}
	Ok((valid, interrupted))
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn invalid_dimensions_and_zero_duration_are_rejected() {
		assert!(parse_resolution("0x1080").is_err());
		assert!(parse_resolution("1920x0").is_err());
		assert!(Args::try_parse_from(["bench", "--duration", "0", "--", "vkcube"]).is_err());
		assert!(Args::try_parse_from(["bench", "--fps", "0", "--", "vkcube"]).is_err());
	}
}
