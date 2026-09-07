//! Host-side measurement boundaries. No timestamps here imply client presentation.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapturePath {
	#[default]
	Composited,
	Direct,
	DirectOverride,
	/// Recovery encode using the previous image, not a new capture.
	Reencode,
}

/// Monotonic counters survive lost stats messages. Static ticks are not drops.
#[derive(Clone, Copy)]
pub(crate) enum Counter {
	CaptureTicks,
	StaticSkips,
	PoolBusy,
	CaptureAttempts,
	CaptureExported,
	CaptureQueueFull,
	CaptureErrors,
	EncodeBackpressure,
	ImportErrors,
	ConvertErrors,
	SubmitErrors,
	Submitted,
	ReadbackErrors,
	PacketizeErrors,
	NoClient,
	SendFrames,
	SendFailedFrames,
	SendDatagrams,
	SendBytes,
	SendErrors,
	GsoFallbackChunks,
	CaptureVisibleUpdate,
}
const COUNTER_NAMES: [&str; 22] = [
	"capture_ticks",
	"static_skips",
	"pool_busy",
	"capture_attempts",
	"capture_exported",
	"capture_queue_full",
	"capture_errors",
	"encode_backpressure",
	"import_errors",
	"convert_errors",
	"submit_errors",
	"submitted",
	"readback_errors",
	"packetize_errors",
	"no_client",
	"send_frames",
	"send_failed_frames",
	"send_datagrams",
	"send_bytes",
	"send_errors",
	"gso_fallback_chunks",
	"capture_visible_update",
];

#[derive(Clone)]
pub(crate) struct VideoDiagnostics {
	frames: broadcast::Sender<FrameStats>,
	counters: Arc<[AtomicU64; COUNTER_NAMES.len()]>,
}

impl Default for VideoDiagnostics {
	fn default() -> Self {
		Self {
			frames: broadcast::channel(1024).0,
			counters: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
		}
	}
}

impl VideoDiagnostics {
	pub fn count(&self, counter: Counter) {
		self.add(counter, 1);
	}
	pub fn add(&self, counter: Counter, value: u64) {
		self.counters[counter as usize].fetch_add(value, Ordering::Relaxed);
	}
	pub fn snapshot(&self) -> BTreeMap<String, u64> {
		COUNTER_NAMES
			.iter()
			.zip(self.counters.iter())
			.map(|(name, value)| (name.to_string(), value.load(Ordering::Relaxed)))
			.collect()
	}
	pub fn subscribe(&self) -> broadcast::Receiver<FrameStats> {
		self.frames.subscribe()
	}
	pub fn send(&self, stats: FrameStats) {
		let _ = self.frames.send(stats);
	}
}

/// One encoded frame, emitted only after all socket send attempts finish.
/// Durations use the host monotonic clock. Socket completion means kernel
/// acceptance, not delivery to the NIC, client, decoder, or display.
#[derive(Clone, Debug)]
pub struct FrameStats {
	/// Frame number in the GameStream packet headers (resets on reconnect).
	pub frame_number: u32,
	pub capture_path: CapturePath,
	pub capture_started_at: Instant,
	pub completed_at: Instant,
	/// Applied attachment of the selected direct-capture buffer until capture.
	/// Absent for composition/reencode. Does not include client rendering or
	/// time spent attached but not committed; GPU readiness is not implied.
	pub buffer_age: Option<Duration>,
	/// First observed scene invalidation until capture begins. Diagnostic:
	/// not the render/commit time of a particular application's buffer.
	pub scene_wait: Option<Duration>,
	/// Actual capture start minus scheduled timer deadline.
	pub timer_lateness: Duration,
	/// Capture start through render/scanout export completion.
	pub capture: Duration,
	/// CPU wait for the compositor render fence; included in capture.
	pub render_wait: Duration,
	pub channel_wait: Duration,
	pub import: Duration,
	pub convert: Duration,
	pub submit: Duration,
	/// Consumer scheduling/queue time, included in encode_wait.
	pub consumer_queue: Duration,
	/// Submit completion until the encode future is observed ready; includes
	/// GPU work, readback and scheduling, not an isolated GPU duration.
	pub encode_wait: Duration,
	pub packetize: Duration,
	/// Time waiting for capacity in the outgoing frame channel.
	pub send: Duration,
	/// Channel publication until the sender dequeues this frame.
	pub network_queue: Duration,
	/// First socket attempt through completion of all attempts for the frame.
	pub network_send: Duration,
	/// Capture start through socket send completion (excludes scene_wait).
	pub total: Duration,
	/// Queue occupancy just before publication, excluding this frame.
	pub network_queue_depth: usize,
	/// Whether every datagram was accepted by the socket.
	pub send_success: bool,
	pub send_errors: u64,
	pub sent_bytes: u64,
	pub sent_datagrams: u64,
	pub gso_fallback_chunks: u64,
	pub encoded_bytes: usize,
	pub is_key_frame: bool,
}

impl FrameStats {
	/// Additive stages. Diagnostic subsets are intentionally excluded.
	pub fn accounted(&self) -> Duration {
		self.capture
			+ self.channel_wait
			+ self.import
			+ self.convert
			+ self.submit
			+ self.encode_wait
			+ self.packetize
			+ self.send
			+ self.network_queue
			+ self.network_send
	}
}

#[cfg(test)]
pub(crate) fn test_frame(start: Instant) -> FrameStats {
	FrameStats {
		frame_number: 1,
		capture_path: CapturePath::Composited,
		capture_started_at: start,
		completed_at: start,
		scene_wait: None,
		buffer_age: None,
		timer_lateness: Duration::ZERO,
		capture: Duration::from_millis(2),
		render_wait: Duration::from_millis(1),
		channel_wait: Duration::from_millis(1),
		import: Duration::from_millis(1),
		convert: Duration::from_millis(2),
		submit: Duration::from_millis(1),
		consumer_queue: Duration::from_millis(1),
		encode_wait: Duration::from_millis(2),
		packetize: Duration::from_millis(1),
		send: Duration::from_millis(1),
		network_queue: Duration::ZERO,
		network_send: Duration::ZERO,
		total: Duration::ZERO,
		network_queue_depth: 0,
		send_success: false,
		send_errors: 0,
		sent_bytes: 0,
		sent_datagrams: 0,
		gso_fallback_chunks: 0,
		encoded_bytes: 100,
		is_key_frame: false,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn snapshots_are_monotonic_across_clones() {
		let diagnostics = VideoDiagnostics::default();
		let producer = diagnostics.clone();
		producer.count(Counter::CaptureQueueFull);
		producer.add(Counter::SendBytes, 1400);
		assert_eq!(diagnostics.snapshot()["capture_queue_full"], 1);
		assert_eq!(diagnostics.snapshot()["send_bytes"], 1400);
		assert_eq!(diagnostics.snapshot()["static_skips"], 0);
	}
}
