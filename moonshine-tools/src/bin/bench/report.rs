use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;
use moonshine_core::session::stream::video::{CapturePath, FrameStats};
use serde::Serialize;

pub fn us(duration: Duration) -> u64 {
	duration.as_micros().min(u64::MAX as u128) as u64
}

/// Fixed histogram range bounds memory even for indefinite runs. Out-of-range
/// values are counted explicitly; extrema and mean still use the actual values.
struct Distribution {
	histogram: Histogram<u64>,
	sum: u128,
	count: u64,
	min: u64,
	max: u64,
	overflow: u64,
}
impl Default for Distribution {
	fn default() -> Self {
		Self {
			histogram: Histogram::new_with_bounds(1, 60_000_000, 3).unwrap(),
			sum: 0,
			count: 0,
			min: u64::MAX,
			max: 0,
			overflow: 0,
		}
	}
}
impl Distribution {
	fn add(&mut self, value: u64) {
		self.sum += value as u128;
		self.count += 1;
		self.min = self.min.min(value);
		self.max = self.max.max(value);
		if self.histogram.record(value).is_err() {
			self.overflow += 1;
		}
	}
	fn summary(&self) -> DistributionSummary {
		// Never publish misleading percentiles if any value exceeded the range.
		let percentile = |q| {
			(self.overflow == 0 && self.count > 0)
				.then(|| self.histogram.value_at_quantile(q).min(self.max).max(self.min))
		};
		DistributionSummary {
			count: self.count,
			mean: if self.count == 0 {
				0.0
			} else {
				self.sum as f64 / self.count as f64
			},
			min: if self.count == 0 { 0 } else { self.min },
			max: self.max,
			p50: percentile(0.50),
			p95: percentile(0.95),
			p99: percentile(0.99),
			overflow: self.overflow,
		}
	}
}

#[derive(Debug, Serialize)]
pub struct DistributionSummary {
	pub count: u64,
	pub mean: f64,
	pub min: u64,
	pub max: u64,
	pub p50: Option<u64>,
	pub p95: Option<u64>,
	pub p99: Option<u64>,
	pub overflow: u64,
}

pub fn stages(stats: &FrameStats) -> BTreeMap<&'static str, u64> {
	let mut result = BTreeMap::from([
		("host_total", us(stats.total)),
		("capture", us(stats.capture)),
		("channel_wait", us(stats.channel_wait)),
		("import", us(stats.import)),
		("convert", us(stats.convert)),
		("submit", us(stats.submit)),
		("encode_wait", us(stats.encode_wait)),
		("packetize", us(stats.packetize)),
		("enqueue_wait", us(stats.send)),
		("network_queue", us(stats.network_queue)),
		("network_send", us(stats.network_send)),
		("timer_lateness", us(stats.timer_lateness)),
		("render_wait", us(stats.render_wait)),
		("consumer_queue", us(stats.consumer_queue)),
		("unaccounted", us(stats.total.saturating_sub(stats.accounted()))),
	]);
	if let Some(age) = stats.buffer_age {
		result.insert("buffer_age", us(age));
		result.insert("buffer_to_send", us(age + stats.total));
	}
	if let Some(wait) = stats.scene_wait {
		result.insert("scene_wait", us(wait));
	}
	result
}

#[derive(Default)]
pub struct Accumulator {
	stages: BTreeMap<&'static str, Distribution>,
	groups: BTreeMap<String, Distribution>,
	queue_depth: Distribution,
	pub successful_frames: u64,
	pub failed_frames: u64,
	pub reencoded_frames: u64,
	pub over_budget: u64,
	encoded_bytes: u64,
	sent_bytes: u64,
	previous_completion: Option<Instant>,
	pub incomplete_receipts: u64,
}
impl Accumulator {
	pub fn add(&mut self, stats: &FrameStats, fps: u32) {
		if !stats.send_success {
			self.failed_frames += 1;
			return;
		}
		let group = format!(
			"{:?}/{}",
			stats.capture_path,
			if stats.is_key_frame { "key" } else { "inter" }
		);
		self.groups.entry(group).or_default().add(us(stats.total));
		if stats.capture_path == CapturePath::Reencode {
			self.reencoded_frames += 1;
			return;
		}
		self.successful_frames += 1;
		self.encoded_bytes += stats.encoded_bytes as u64;
		self.sent_bytes += stats.sent_bytes;
		self.over_budget += u64::from(stats.total > Duration::from_secs_f64(1.0 / fps as f64));
		for (stage, value) in stages(stats) {
			self.record(stage, value);
		}
		self.queue_depth.add(stats.network_queue_depth as u64);
		if let Some(previous) = self.previous_completion {
			self.record("send_interval", us(stats.completed_at.duration_since(previous)));
		}
		self.previous_completion = Some(stats.completed_at);
	}
	pub fn record(&mut self, stage: &'static str, value: u64) {
		self.stages.entry(stage).or_default().add(value);
	}
	pub fn summary(&self, elapsed: Duration) -> Summary {
		let seconds = elapsed.as_secs_f64();
		Summary {
			measured_seconds: seconds,
			successful_frames: self.successful_frames,
			failed_frames: self.failed_frames,
			reencoded_frames: self.reencoded_frames,
			successful_fps: if seconds > 0.0 {
				self.successful_frames as f64 / seconds
			} else {
				0.0
			},
			encoded_mbps: if seconds > 0.0 {
				self.encoded_bytes as f64 * 8.0 / seconds / 1e6
			} else {
				0.0
			},
			socket_mbps: if seconds > 0.0 {
				self.sent_bytes as f64 * 8.0 / seconds / 1e6
			} else {
				0.0
			},
			over_frame_budget: self.over_budget,
			incomplete_receipts: self.incomplete_receipts,
			stages_us: self.stages.iter().map(|(k, v)| (*k, v.summary())).collect(),
			host_total_by_path_and_frame_type_us: self.groups.iter().map(|(k, v)| (k.clone(), v.summary())).collect(),
			network_queue_depth_frames: self.queue_depth.summary(),
		}
	}
}

#[derive(Serialize)]
pub struct Summary {
	pub measured_seconds: f64,
	pub successful_frames: u64,
	pub failed_frames: u64,
	pub reencoded_frames: u64,
	pub successful_fps: f64,
	pub encoded_mbps: f64,
	pub socket_mbps: f64,
	pub over_frame_budget: u64,
	pub incomplete_receipts: u64,
	pub stages_us: BTreeMap<&'static str, DistributionSummary>,
	pub host_total_by_path_and_frame_type_us: BTreeMap<String, DistributionSummary>,
	pub network_queue_depth_frames: DistributionSummary,
}
impl Summary {
	pub fn print(&self) {
		tracing::info!(
			"{} successful captures, {:.2} FPS, {:.2} encoded Mbps, {} over budget, {} failed, {} reencoded",
			self.successful_frames,
			self.successful_fps,
			self.encoded_mbps,
			self.over_frame_budget,
			self.failed_frames,
			self.reencoded_frames
		);
		tracing::info!("Host stages (us): mean / p50 / p95 / p99 / max; diagnostic subsets must not be added");
		for (stage, d) in &self.stages_us {
			tracing::info!(
				"{stage:>18}: {:.1} / {:?} / {:?} / {:?} / {} (n={})",
				d.mean,
				d.p50,
				d.p95,
				d.p99,
				d.max,
				d.count
			);
		}
	}
}

pub fn counter_delta(end: &BTreeMap<String, u64>, start: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
	end.iter()
		.map(|(key, value)| (key.clone(), value.saturating_sub(*start.get(key).unwrap_or(&0))))
		.collect()
}

/// Submitted frames without a terminal outcome at the counter snapshot.
/// This exposes work censored by the end of the measurement window.
pub fn outstanding(counters: &BTreeMap<String, u64>) -> u64 {
	counters.get("submitted").copied().unwrap_or(0).saturating_sub(
		["readback_errors", "packetize_errors", "no_client", "send_frames"]
			.iter()
			.map(|key| counters.get(*key).copied().unwrap_or(0))
			.sum(),
	)
}

#[cfg(test)]
fn test_frame(start: Instant) -> FrameStats {
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
	fn buffer_to_send_is_summed_per_frame_and_absent_without_a_source_timestamp() {
		let mut frame = test_frame(Instant::now());
		assert!(!stages(&frame).contains_key("buffer_age"));
		assert!(!stages(&frame).contains_key("buffer_to_send"));
		frame.buffer_age = Some(Duration::from_micros(16_000));
		frame.total = Duration::from_micros(3_000);
		assert_eq!(stages(&frame)["buffer_to_send"], 19_000);
	}

	#[test]
	fn tails_and_overflow_are_not_hidden() {
		let mut distribution = Distribution::default();
		for value in 1..=100 {
			distribution.add(value);
		}
		let s = distribution.summary();
		assert_eq!((s.p50, s.p95, s.p99), (Some(50), Some(95), Some(99)));
		let mut one = Distribution::default();
		one.add(2048);
		assert_eq!(
			one.summary().p99,
			Some(2048),
			"quantiles must not exceed the exact observed maximum"
		);
		distribution.add(120_000_000);
		let s = distribution.summary();
		assert_eq!(s.max, 120_000_000);
		assert_eq!(s.overflow, 1);
		assert_eq!(s.p99, None);
	}
	#[test]
	fn counter_windows_do_not_include_warmup() {
		let start = BTreeMap::from([("static_skips".into(), 10), ("capture_queue_full".into(), 3)]);
		let end = BTreeMap::from([("static_skips".into(), 12), ("capture_queue_full".into(), 8)]);
		let delta = counter_delta(&end, &start);
		assert_eq!(delta["static_skips"], 2);
		assert_eq!(delta["capture_queue_full"], 5);
	}
}

#[cfg(test)]
mod accounting_tests {
	use super::*;
	#[test]
	fn failures_and_recovery_frames_cannot_improve_capture_latency_or_fps() {
		let start = Instant::now();
		let mut accumulator = Accumulator::default();
		let mut frame = test_frame(start);
		frame.send_success = true;
		frame.total = Duration::from_millis(20);
		accumulator.add(&frame, 60);
		frame.send_success = false;
		frame.total = Duration::from_micros(1);
		accumulator.add(&frame, 60);
		frame.send_success = true;
		frame.capture_path = CapturePath::Reencode;
		accumulator.add(&frame, 60);
		let summary = accumulator.summary(Duration::from_secs(2));
		assert_eq!(summary.successful_frames, 1);
		assert_eq!(summary.successful_fps, 0.5);
		assert_eq!(summary.failed_frames, 1);
		assert_eq!(summary.reencoded_frames, 1);
		assert_eq!(summary.over_frame_budget, 1);
		assert_eq!(summary.stages_us["host_total"].mean, 20_000.0);
	}
}
