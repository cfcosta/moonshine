use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::{self, SyncSender};
use std::thread::JoinHandle;
use std::time::Instant;

use super::report::{stages, us};
use moonshine_core::session::stream::video::FrameStats;
use serde_json::json;

/// Disk serialization never blocks the subscriber or the streaming pipeline.
/// Overflow is surfaced in the run's validity flags, not silently ignored.
pub struct RawWriter {
	sender: SyncSender<FrameStats>,
	worker: JoinHandle<io::Result<()>>,
	pub dropped: u64,
}
impl RawWriter {
	pub fn new(path: &Path, origin: Instant) -> io::Result<Self> {
		let file = File::create_new(path)?;
		let (sender, receiver) = mpsc::sync_channel::<FrameStats>(4096);
		let worker = std::thread::Builder::new().name("bench-jsonl".into()).spawn(move || {
			let mut out = BufWriter::new(file);
			for stats in receiver {
				serde_json::to_writer(
					&mut out,
					&json!({
						"schema_version": 1, "frame_number": stats.frame_number,
						"capture_start_us": us(stats.capture_started_at.saturating_duration_since(origin)),
						"socket_complete_us": us(stats.completed_at.saturating_duration_since(origin)),
						"capture_path": stats.capture_path, "key_frame": stats.is_key_frame,
						"send_success": stats.send_success, "send_errors": stats.send_errors,
						"encoded_bytes": stats.encoded_bytes, "sent_bytes": stats.sent_bytes,
						"sent_datagrams": stats.sent_datagrams, "gso_fallback_chunks": stats.gso_fallback_chunks,
						"network_queue_depth": stats.network_queue_depth, "stages_us": stages(&stats),
					}),
				)?;
				out.write_all(b"\n")?;
			}
			out.flush()
		})?;
		Ok(Self {
			sender,
			worker,
			dropped: 0,
		})
	}
	pub fn record(&mut self, frame: &FrameStats) {
		if self.sender.try_send(frame.clone()).is_err() {
			self.dropped += 1;
		}
	}
	pub fn finish(self) -> io::Result<u64> {
		drop(self.sender);
		self.worker
			.join()
			.map_err(|_| io::Error::other("raw writer panicked"))??;
		Ok(self.dropped)
	}
}
