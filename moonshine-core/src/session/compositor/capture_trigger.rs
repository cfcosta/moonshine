//! Applied content changes, filtered against the scene selected for capture.
//!
//! The timer owns callback pacing; applied visible content can use its capture
//! opportunity immediately after client dispatch. Geometry/focus/cursor invalidation
//! remains a separate reason to repaint, not evidence of a new buffer.

use std::collections::{HashMap, HashSet};

use smithay::backend::renderer::element::{Id, RenderElementStates};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::{
	BufferAssignment, SurfaceAttributes, TraversalAction, is_sync_subsurface, with_surface_tree_upward,
};

struct ContentUpdate {
	removed: bool,
	revision: u64,
}

#[derive(Default)]
pub(super) struct CaptureTrigger {
	// Coalesce commits until a frame is accepted, retaining the latest batch.
	pending: HashMap<Id, ContentUpdate>,
	previously_visible: HashSet<Id>,
	revision: u64,
}

impl CaptureTrigger {
	/// Inspect exactly the applied state that on_commit_buffer_handler will
	/// consume, including children latched by this parent commit. A sync child's
	/// own commit must not make its cached buffer eligible prematurely.
	pub fn observe_commit(&mut self, surface: &WlSurface) {
		if is_sync_subsurface(surface) {
			return;
		}
		with_surface_tree_upward(
			surface,
			(),
			|_, _, _| TraversalAction::DoChildren(()),
			|surface, states, _| {
				let mut attrs = states.cached_state.get::<SurfaceAttributes>();
				let attrs = attrs.current();
				if attrs.buffer.is_some() || !attrs.damage.is_empty() {
					self.revision += 1;
					let removed = matches!(attrs.buffer, Some(BufferAssignment::Removed));
					self.pending
						.entry(Id::from_wayland_resource(surface))
						.and_modify(|update| {
							update.removed |= removed;
							update.revision = self.revision;
						})
						.or_insert(ContentUpdate {
							removed,
							revision: self.revision,
						});
				}
			},
			|_, _, _| true,
		);
	}

	pub fn destroyed(&mut self, surface: &WlSurface) -> bool {
		let id = Id::from_wayland_resource(surface);
		let visible = self.previously_visible.contains(&id);
		if visible {
			self.revision += 1;
			self.pending.insert(
				id,
				ContentUpdate {
					removed: true,
					revision: self.revision,
				},
			);
		} else {
			self.pending.remove(&id);
		}
		visible
	}

	/// Changes only for applied content, never callback-only protocol traffic.
	pub fn revision(&self) -> u64 {
		self.revision
	}

	/// Query without consuming: failed exports/backpressure must retain readiness.
	pub fn ready_for(&self, scene: &RenderElementStates) -> bool {
		self.ready_for_since(scene, 0)
	}

	/// Only content applied in the triggering dispatch may spend its opportunity.
	/// Older pending content still survives for a timer fallback or later capture.
	pub fn ready_for_since(&self, scene: &RenderElementStates, revision: u64) -> bool {
		self.pending.iter().any(|(id, update)| {
			update.revision > revision
				&& (scene.element_was_presented(id.clone()) || (update.removed && self.previously_visible.contains(id)))
		})
	}

	/// Called only after the composed frame was accepted by the encoder channel.
	pub fn accepted(&mut self, scene: &RenderElementStates) -> bool {
		let ready = self.ready_for(scene);
		self.previously_visible = scene
			.states
			.keys()
			.filter(|id| scene.element_was_presented((*id).clone()))
			.cloned()
			.collect();
		self.pending.clear();
		ready
	}

	/// Direct capture has exactly one selected buffer, independent of other
	/// windows/surfaces that may have committed in the same protocol batch.
	pub fn ready_for_direct(&self, surface: &WlSurface) -> bool {
		self.ready_for_direct_since(surface, 0)
	}

	pub fn ready_for_direct_since(&self, surface: &WlSurface, revision: u64) -> bool {
		let selected = Id::from_wayland_resource(surface);
		self.pending.iter().any(|(id, update)| {
			update.revision > revision && (id == &selected || (update.removed && self.previously_visible.contains(id)))
		})
	}

	pub fn accepted_direct(&mut self, surface: &WlSurface) -> bool {
		let ready = self.ready_for_direct(surface);
		self.previously_visible.clear();
		self.previously_visible.insert(Id::from_wayland_resource(surface));
		self.pending.clear();
		ready
	}
}
