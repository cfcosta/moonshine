//! Timestamp the applied buffer attachment, independently of callback-only commits.
//! Only used for direct capture, where the selected source buffer is unambiguous.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use smithay::reexports::wayland_server::{Resource, backend::ObjectId, protocol::wl_surface::WlSurface};
use smithay::wayland::compositor::{BufferAssignment, SurfaceAttributes, is_sync_subsurface, with_states};

#[derive(Default)]
struct BufferCommit(Mutex<Option<(ObjectId, Instant)>>);

/// Call before on_commit_buffer_handler consumes the applied attachment.
/// Synchronized children are deliberately excluded: direct capture selects a
/// toplevel or override surface, never a child with pending synchronized state.
pub(super) fn record(surface: &WlSurface) {
	if is_sync_subsurface(surface) {
		return;
	}
	with_states(surface, |states| {
		let mut attrs = states.cached_state.get::<SurfaceAttributes>();
		let Some(assignment) = &attrs.current().buffer else {
			return;
		};
		states.data_map.insert_if_missing_threadsafe(BufferCommit::default);
		let mut timing = states.data_map.get::<BufferCommit>().unwrap().0.lock().unwrap();
		*timing = match assignment {
			BufferAssignment::NewBuffer(buffer) => Some((buffer.id(), Instant::now())),
			BufferAssignment::Removed => None,
		};
	});
}

pub(super) fn age(surface: &WlSurface, buffer: &ObjectId, capture: Instant) -> Option<Duration> {
	with_states(surface, |states| {
		let timing = states.data_map.get::<BufferCommit>()?.0.lock().unwrap();
		let (id, committed_at) = timing.as_ref()?;
		(id == buffer).then(|| capture.saturating_duration_since(*committed_at))
	})
}
