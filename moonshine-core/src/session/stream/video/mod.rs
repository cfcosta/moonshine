use std::sync::Arc;

use async_shutdown::ShutdownManager;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::session::SessionKeysReceiver;
use crate::session::compositor::frame::{ExportedFrame, HdrModeState};
use crate::session::manager::SessionShutdownReason;

mod gso_socket;
pub(crate) mod metrics;
pub use metrics::{CapturePath, FrameStats};
use metrics::{Counter, VideoDiagnostics};
mod packetizer;
mod pipeline;
mod shard_batch;
use gso_socket::UdpGsoSocket;
use pipeline::VideoPipeline;
use shard_batch::ShardBatch;

/// Configuration for the video stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VideoStreamConfig {
	/// Port to use for streaming video data.
	pub port: u16,

	/// What percentage of data packets should be parity packets.
	pub fec_percentage: u8,

	/// Whether to enable video stream encryption (AES-128-GCM).
	#[serde(default)]
	pub encrypt: bool,

	/// Whether to emit a WARN log when a single frame takes longer to encode and
	/// packetize than the frame budget.
	#[serde(default)]
	pub log_frame_spikes: bool,
}

impl Default for VideoStreamConfig {
	fn default() -> Self {
		Self {
			port: 47998,
			fec_percentage: 20,
			encrypt: false,
			log_frame_spikes: false,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoFormat {
	#[default]
	H264,
	Hevc,
	Av1,
}

impl TryFrom<u32> for VideoFormat {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(Self::H264),
			1 => Ok(Self::Hevc),
			2 => Ok(Self::Av1),
			_ => Err(()),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoDynamicRange {
	#[default]
	Sdr,
	Hdr,
}

impl TryFrom<u32> for VideoDynamicRange {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(Self::Sdr),
			1 => Ok(Self::Hdr),
			_ => Err(()),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoChromaSampling {
	#[default]
	Yuv420,
	Yuv444,
}

impl TryFrom<u32> for VideoChromaSampling {
	type Error = ();

	fn try_from(value: u32) -> Result<Self, Self::Error> {
		match value {
			0 => Ok(Self::Yuv420),
			1 => Ok(Self::Yuv444),
			_ => Err(()),
		}
	}
}

#[derive(Clone, Debug, Default)]
pub struct VideoStreamContext {
	/// Width of the video stream in pixels.
	pub width: u32,

	/// Height of the video stream in pixels.
	pub height: u32,

	/// Frames per second of the video stream.
	pub fps: u32,

	/// Size of each encoded packet in bytes.
	pub packet_size: usize,

	/// Target bitrate for the video stream in bits per second.
	pub bitrate: usize,

	/// Minimum number of FEC packets to include for each frame.
	pub minimum_fec_packets: u32,

	/// Whether to apply QoS markings to video stream packets.
	pub qos: bool,

	/// Video format to use for encoding the stream.
	pub video_format: VideoFormat,

	/// Dynamic range of the video stream.
	pub dynamic_range: VideoDynamicRange,

	/// Chroma sampling type for the video stream.
	pub chroma_sampling_type: VideoChromaSampling,

	/// Maximum number of reference frames for the video encoder.
	pub max_reference_frames: u32,

	/// Whether the client asked for full-range (0-255) rather than
	/// limited-range (16-235) luma.
	pub full_range: bool,

	/// Whether the client has enabled video encryption.
	pub encrypt_video: bool,
}

/// Handle returned by `VideoStream::start` that gates the pipeline and packet handler.
///
/// The pipeline and packet handler are spawned immediately but block on a `Notify`
/// until `trigger()` is called on `StartB`.
#[derive(Clone)]
pub(crate) struct VideoStreamHandle {
	notify: Arc<Notify>,
	idr_tx: broadcast::Sender<()>,
	/// Reference frame invalidation requests, carrying the inclusive
	/// `[first, last]` client frame-index range the client could not decode.
	invalidate_tx: broadcast::Sender<(u32, u32)>,
	reset_tx: broadcast::Sender<()>,
}

impl VideoStreamHandle {
	/// Signal the video pipeline and packet handler to begin processing.
	pub fn trigger(&self) {
		// Call notify_one() twice instead of notify_waiters() because
		// Notify only wakes tasks already .awaiting; notify_waiters()
		// is a no-op if no task is waiting yet.  notify_one() stores
		// a permit so the next notified().await completes immediately.
		self.notify.notify_one();
		self.notify.notify_one();
	}

	/// Request an IDR (key) frame from the encoder.
	pub fn request_idr_frame(&self) {
		let _ = self.idr_tx.send(());
	}

	/// Request reference frame invalidation for the inclusive client frame-index
	/// range `[first, last]` the client reported it could not decode.
	///
	/// The encoder drops the affected references and recovers by predicting from
	/// a surviving reference where possible, falling back to an IDR only when no
	/// reference survives — much cheaper than always re-sending a keyframe.
	pub fn invalidate_reference_frames(&self, first: u32, last: u32) {
		let _ = self.invalidate_tx.send((first, last));
	}

	/// Reset the stream's frame/sequence counters for a resuming client.
	///
	/// Called when a client reconnects to an already-running session. The pipeline
	/// keeps incrementing `frame_number` for the lifetime of the session, but a fresh
	/// Moonlight session expects frame numbers to start at 1; without a reset it counts
	/// the jump as massive frame loss and reports a poor connection. This also forces an
	/// IDR so the resumed client has a decodable starting frame.
	pub fn request_reset(&self) {
		let _ = self.reset_tx.send(());
	}

	/// Clone the start notify for external triggering (e.g. bench binary).
	pub fn clone_start_notify(&self) -> Arc<Notify> {
		self.notify.clone()
	}
}

/// A frame and its timing stay together through the network queue.
pub(crate) struct PacketFrame {
	shards: ShardBatch,
	stats: FrameStats,
	enqueued_at: std::time::Instant,
}

impl PacketFrame {
	fn complete(
		mut self,
		dequeued_at: std::time::Instant,
		report: gso_socket::SendReport,
		diagnostics: &VideoDiagnostics,
	) {
		let completed_at = std::time::Instant::now();
		self.stats.completed_at = completed_at;
		self.stats.network_queue = dequeued_at.duration_since(self.enqueued_at);
		self.stats.network_send = completed_at.duration_since(dequeued_at);
		self.stats.total = completed_at.duration_since(self.stats.capture_started_at);
		self.stats.send_success = report.errors == 0;
		self.stats.send_errors = report.errors;
		self.stats.sent_bytes = report.bytes;
		self.stats.sent_datagrams = report.datagrams;
		self.stats.gso_fallback_chunks = report.fallback_chunks;
		diagnostics.count(Counter::SendFrames);
		if report.errors > 0 {
			diagnostics.count(Counter::SendFailedFrames);
		}
		diagnostics.add(Counter::SendErrors, report.errors);
		diagnostics.add(Counter::SendBytes, report.bytes);
		diagnostics.add(Counter::SendDatagrams, report.datagrams);
		diagnostics.add(Counter::GsoFallbackChunks, report.fallback_chunks);
		diagnostics.send(self.stats);
	}
}

pub(crate) struct VideoStream {
	socket: UdpGsoSocket,
	frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
	hdr_metadata_tx: watch::Sender<HdrModeState>,
	stats_tx: VideoDiagnostics,
}

impl VideoStream {
	pub async fn new(
		config: VideoStreamConfig,
		address: String,
		frame_rx: std::sync::mpsc::Receiver<ExportedFrame>,
		hdr_metadata_tx: watch::Sender<HdrModeState>,
		_stop: ShutdownManager<SessionShutdownReason>,
		stats_tx: VideoDiagnostics,
	) -> Result<Self, ()> {
		tracing::debug!("Initializing video stream.");

		let socket = UdpGsoSocket::new(&address, config.port).await?;

		tracing::debug!(
			"Listening for video messages on {}",
			socket
				.local_addr()
				.map_err(|e| tracing::warn!("Failed to get local address associated with video socket: {e}"))?
		);

		Ok(Self {
			socket,
			frame_rx,
			hdr_metadata_tx,
			stats_tx,
		})
	}

	#[allow(clippy::too_many_arguments)]
	pub fn start(
		self,
		config: VideoStreamConfig,
		context: VideoStreamContext,
		keys_rx: SessionKeysReceiver,
		stop: ShutdownManager<SessionShutdownReason>,
	) -> Result<VideoStreamHandle, ()> {
		let Self {
			socket,
			frame_rx,
			hdr_metadata_tx,
			stats_tx,
		} = self;

		// Apply QoS to UDP socket.
		if context.qos {
			let _ = socket.set_tos_v4(160);
		}

		// Gate for pipeline + packet handler.
		let start_notify = Arc::new(Notify::new());

		// IDR broadcast channel.
		let (idr_tx, _idr_rx) = broadcast::channel(1);

		// Reference frame invalidation broadcast channel. Sized for a small burst
		// of loss reports; the encode loop drains all pending each iteration.
		let (invalidate_tx, _invalidate_rx) = broadcast::channel(16);

		// Stream-reset broadcast channel (client reconnect/resume).
		let (reset_tx, _reset_rx) = broadcast::channel(1);

		// Packet channel.
		let (packet_tx, packet_rx) = mpsc::channel::<PacketFrame>(128);

		// Spawn packet handler — gated behind start_notify.
		spawn_handle_video_packets(packet_rx, socket, start_notify.clone(), stop.clone(), stats_tx.clone());

		// Spawn pipeline thread — gated behind start_notify.
		VideoPipeline::new(
			frame_rx,
			config,
			context,
			keys_rx,
			packet_tx,
			idr_tx.clone(),
			idr_tx.subscribe(),
			invalidate_tx.subscribe(),
			reset_tx.subscribe(),
			stop.clone(),
			hdr_metadata_tx,
			start_notify.clone(),
			stats_tx,
		)
		.map_err(|()| tracing::error!("Failed to create video pipeline"))?;

		Ok(VideoStreamHandle {
			notify: start_notify,
			idr_tx,
			invalidate_tx,
			reset_tx,
		})
	}
}

fn spawn_handle_video_packets(
	mut packet_rx: mpsc::Receiver<PacketFrame>,
	socket: UdpGsoSocket,
	start: Arc<Notify>,
	stop_session_manager: ShutdownManager<SessionShutdownReason>,
	diagnostics: VideoDiagnostics,
) {
	tokio::spawn(async move {
		start.notified().await;

		let mut buf = [0; 1024];
		let mut client_address = None;
		// Rate-limits the GSO-fallback warning.
		let mut last_send_warn: Option<std::time::Instant> = None;

		// Trigger session shutdown if we exit unexpectedly.
		let _stop_token = stop_session_manager.trigger_shutdown_token(SessionShutdownReason::VideoPacketHandlerStopped);
		let _delay_stop = stop_session_manager.delay_shutdown_token();

		while !stop_session_manager.is_shutdown_triggered() {
			tokio::select! {
				batch = stop_session_manager.wrap_cancel(packet_rx.recv()) => {
					match batch {
						Ok(Some(frame)) => {
							let dequeued_at = std::time::Instant::now();
							let batch = &frame.shards;
							if let Some(addr) = client_address {
								if batch.shard_count() == 0 {
									continue;
								}

								// Sends are wrapped in wrap_cancel so a socket that
								// stops draining cannot block session shutdown.
								match stop_session_manager
									.wrap_cancel(socket.send_batch(batch, addr))
									.await
								{
									Ok(report) => {
										let failed_chunks = report.fallback_chunks;
										frame.complete(dequeued_at, report, &diagnostics);
										if failed_chunks > 0
											&& last_send_warn
												.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
										{
											tracing::warn!(
												"GSO send failed for {failed_chunks} chunk(s), sent per-shard instead"
											);
											last_send_warn = Some(std::time::Instant::now());
										}
									},
									Err(_) => break,
								}
							} else { diagnostics.count(Counter::NoClient); }
						},
						Ok(None) => {
							tracing::debug!("Video packet channel closed.");
							break;
						},
						Err(_) => break,
					}
				},

				message = stop_session_manager.wrap_cancel(socket.recv_from(&mut buf)) => {
					let (len, address) = match message {
						Ok(Ok((len, address))) => (len, address),
						Ok(Err(e)) => {
							tracing::warn!("Failed to receive message: {e}");
							break;
						},
						Err(_) => break,
					};

					if &buf[..len] == b"PING" {
						tracing::trace!("Received video stream PING message from {address}.");
						client_address = Some(address);
					} else {
						tracing::warn!("Received unknown message on video stream of length {len}.");
					}
				},
			}
		}

		tracing::debug!("Video packet stream stopped.");
	});
}

#[cfg(test)]
mod measurement_tests {
	use super::*;
	use std::time::{Duration, Instant};

	#[test]
	fn socket_completion_includes_queue_wait_and_does_not_double_count_subsets() {
		let diagnostics = VideoDiagnostics::default();
		let mut rx = diagnostics.subscribe();
		let start = Instant::now() - Duration::from_millis(30);
		let stats = metrics::test_frame(start);
		assert_eq!(stats.accounted(), Duration::from_millis(11));
		let frame = PacketFrame {
			shards: ShardBatch::empty(),
			stats,
			enqueued_at: start + Duration::from_millis(11),
		};
		assert!(rx.try_recv().is_err(), "no completed sample at enqueue");
		frame.complete(
			start + Duration::from_millis(21),
			gso_socket::SendReport {
				bytes: 1400,
				datagrams: 1,
				..Default::default()
			},
			&diagnostics,
		);
		let stats = rx.try_recv().unwrap();
		assert_eq!(stats.network_queue, Duration::from_millis(10));
		assert!(stats.total >= Duration::from_millis(30));
		assert_eq!(stats.total, stats.accounted());
		assert!(stats.send_success);
		assert_eq!(diagnostics.snapshot()["send_bytes"], 1400);
	}

	#[test]
	fn send_failures_remain_observable() {
		let diagnostics = VideoDiagnostics::default();
		let mut rx = diagnostics.subscribe();
		let start = Instant::now() - Duration::from_millis(20);
		PacketFrame {
			shards: ShardBatch::empty(),
			stats: metrics::test_frame(start),
			enqueued_at: start + Duration::from_millis(11),
		}
		.complete(
			Instant::now(),
			gso_socket::SendReport {
				errors: 2,
				..Default::default()
			},
			&diagnostics,
		);
		assert!(!rx.try_recv().unwrap().send_success);
		assert_eq!(diagnostics.snapshot()["send_errors"], 2);
		assert_eq!(diagnostics.snapshot()["send_failed_frames"], 1);
	}
}
