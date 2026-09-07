//! Loopback observation, on a separate thread from encoding and the Tokio sender.
//! This validates receipt of every emitted shard; it is not a decoder or client.
use std::collections::{BTreeMap, HashSet};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::Serialize;

pub struct Receipt {
	pub first: Instant,
	pub last: Instant,
	indices: HashSet<u32>,
}
#[derive(Default, Clone, Serialize)]
pub struct ReceiverCounters {
	pub datagrams: u64,
	pub bytes: u64,
	pub duplicates: u64,
	pub malformed: u64,
	pub evicted_frames: u64,
	pub errors: u64,
}
#[derive(Default)]
struct State {
	frames: BTreeMap<u32, Receipt>,
	counters: ReceiverCounters,
}
impl State {
	fn record(&mut self, bytes: &[u8], at: Instant) {
		self.counters.datagrams += 1;
		self.counters.bytes += bytes.len() as u64;
		// Benchmark sends unencrypted GameStream video: RTP(12), padding(4),
		// stream packet index(4), frame index(4), remaining NV header(8).
		if bytes.len() < 32 || bytes[0] != 0x90 {
			self.counters.malformed += 1;
			return;
		}
		let frame = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
		let index = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
		let entry = self.frames.entry(frame).or_insert_with(|| Receipt {
			first: at,
			last: at,
			indices: HashSet::new(),
		});
		if entry.indices.insert(index) {
			entry.last = at;
		} else {
			self.counters.duplicates += 1;
		}
		// Bound memory if the stats subscriber stalls or frames never complete.
		if self.frames.len() > 1024 {
			self.frames.pop_first();
			self.counters.evicted_frames += 1;
		}
	}
}

pub struct Receiver {
	state: Arc<Mutex<State>>,
	stop: Arc<AtomicBool>,
	worker: Option<JoinHandle<()>>,
}
impl Receiver {
	pub fn start(server: SocketAddr) -> io::Result<Self> {
		let socket = UdpSocket::bind("127.0.0.1:0")?;
		socket.set_read_timeout(Some(Duration::from_millis(10)))?;
		socket.send_to(b"PING", server)?;
		let state = Arc::new(Mutex::new(State::default()));
		let stop = Arc::new(AtomicBool::new(false));
		let worker = {
			let state = state.clone();
			let stop = stop.clone();
			std::thread::Builder::new()
				.name("bench-receiver".into())
				.spawn(move || {
					let mut bytes = [0; 65536];
					let mut ping = Instant::now();
					while !stop.load(Ordering::Relaxed) {
						match socket.recv_from(&mut bytes) {
							Ok((len, source)) if source == server => {
								let at = Instant::now();
								state.lock().unwrap().record(&bytes[..len], at);
							},
							Ok(_) => {},
							Err(e)
								if matches!(
									e.kind(),
									io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
								) => {},
							Err(_) => {
								state.lock().unwrap().counters.errors += 1;
								break;
							},
						}
						if ping.elapsed() >= Duration::from_secs(1) {
							if socket.send_to(b"PING", server).is_err() {
								state.lock().unwrap().counters.errors += 1;
							}
							ping = Instant::now();
						}
					}
				})?
		};
		Ok(Self {
			state,
			stop,
			worker: Some(worker),
		})
	}
	pub fn take_complete(&self, frame: u32, expected: u64) -> Option<Receipt> {
		let mut state = self.state.lock().unwrap();
		if state
			.frames
			.get(&frame)
			.is_some_and(|r| r.indices.len() as u64 == expected)
		{
			state.frames.remove(&frame)
		} else {
			None
		}
	}
	pub fn discard(&self, frame: u32) {
		self.state.lock().unwrap().frames.remove(&frame);
	}
	pub fn counters(&self) -> ReceiverCounters {
		self.state.lock().unwrap().counters.clone()
	}
}
impl Drop for Receiver {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		if let Some(worker) = self.worker.take() {
			let _ = worker.join();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn receipt_requires_unique_shards_and_tracks_last_arrival() {
		let mut state = State::default();
		let mut packet = [0; 32];
		packet[0] = 0x90;
		packet[20..24].copy_from_slice(&7u32.to_le_bytes());
		let start = Instant::now();
		state.record(&packet, start);
		state.record(&packet, start + Duration::from_millis(2));
		assert_eq!(state.frames[&7].indices.len(), 1);
		assert_eq!(state.frames[&7].last, start);
		packet[16..20].copy_from_slice(&256u32.to_le_bytes());
		state.record(&packet, start + Duration::from_millis(3));
		assert_eq!(state.frames[&7].indices.len(), 2);
		assert_eq!(state.frames[&7].last.duration_since(start), Duration::from_millis(3));
		assert_eq!(state.counters.duplicates, 1);
	}
	#[test]
	fn receiver_stays_alive_and_receives_real_udp() {
		let server = UdpSocket::bind("127.0.0.1:0").unwrap();
		server.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
		let receiver = Receiver::start(server.local_addr().unwrap()).unwrap();
		let mut ping = [0; 4];
		let (_, destination) = server.recv_from(&mut ping).unwrap();
		assert_eq!(&ping, b"PING");
		let mut packet = [0; 32];
		packet[0] = 0x90;
		packet[20..24].copy_from_slice(&1u32.to_le_bytes());
		server.send_to(&packet, destination).unwrap();
		let deadline = Instant::now() + Duration::from_secs(1);
		loop {
			if receiver.take_complete(1, 1).is_some() {
				break;
			}
			assert!(Instant::now() < deadline);
			std::thread::sleep(Duration::from_millis(1));
		}
		assert_eq!(receiver.counters().datagrams, 1);
	}
}
