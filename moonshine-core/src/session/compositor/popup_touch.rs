//! Touch counterpart to Smithay's popup pointer and keyboard grabs.
//!
//! Smithay retains each contact's surface and origin inside TouchInnerHandle.
//! Keeping that state intact also preserves contacts that began before a menu
//! opened, while each new contact uses normal owner-events hit testing.

use smithay::backend::input::TouchSlot;
use smithay::desktop::{PopupGrab, PopupUngrabStrategy};
use smithay::input::touch::{
	DefaultGrab, DownEvent, GrabStartData, MotionEvent, OrientationEvent, ShapeEvent,
	TouchGrab, TouchInnerHandle, UpEvent,
};
use smithay::reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface};
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::seat::WaylandFocus;

use super::state::MoonshineCompositor;

#[derive(Debug)]
pub(super) struct PopupTouchGrab {
	popup_grab: PopupGrab<MoonshineCompositor>,
	start_data: GrabStartData<MoonshineCompositor>,
}

impl PopupTouchGrab {
	pub(super) fn new(popup_grab: &PopupGrab<MoonshineCompositor>) -> Self {
		Self {
			popup_grab: popup_grab.clone(),
			start_data: GrabStartData {
				// Keep the grab alive until its root is destroyed, even when the
				// submenu that was current at installation has gone away.
				focus: popup_grab.pointer_grab_start_data().focus.clone(),
				slot: TouchSlot::from(None),
				location: (0.0, 0.0).into(),
			},
		}
	}

	fn finish_if_ended(&mut self, data: &mut MoonshineCompositor, handle: &mut TouchInnerHandle<'_, MoonshineCompositor>) {
		if self.popup_grab.has_ended() {
			handle.unset_grab(self, data);
		}
	}
}

impl TouchGrab<MoonshineCompositor> for PopupTouchGrab {
	fn down(
		&mut self,
		data: &mut MoonshineCompositor,
		handle: &mut TouchInnerHandle<'_, MoonshineCompositor>,
		focus: Option<(WlSurface, Point<f64, Logical>)>,
		event: &DownEvent,
		seq: Serial,
	) {
		if self.popup_grab.has_ended() {
			handle.unset_grab(self, data);
			DefaultGrab.down(data, handle, focus, event, seq);
			return;
		}

		let same_client = focus.as_ref().is_some_and(|(surface, _)| {
			self.popup_grab.current_grab().is_some_and(|grab| {
				grab.wl_surface().is_some_and(|root| surface.same_client_as(&root.id()))
			})
		});
		if same_client {
			handle.down(data, focus, event, seq);
		} else {
			// The triggering touch belongs to normal input after dismissal.
			// The other seat grabs are reconciled by the compositor once this
			// handler returns and the touch mutex has been released.
			self.popup_grab.ungrab(PopupUngrabStrategy::All);
			data.screen_dirty = true;
			handle.unset_grab(self, data);
			DefaultGrab.down(data, handle, focus, event, seq);
		}
	}

	fn up(&mut self, data: &mut MoonshineCompositor, handle: &mut TouchInnerHandle<'_, MoonshineCompositor>, event: &UpEvent, seq: Serial) {
		self.finish_if_ended(data, handle);
		handle.up(data, event, seq);
	}

	fn motion(
		&mut self,
		data: &mut MoonshineCompositor,
		handle: &mut TouchInnerHandle<'_, MoonshineCompositor>,
		focus: Option<(WlSurface, Point<f64, Logical>)>,
		event: &MotionEvent,
		seq: Serial,
	) {
		self.finish_if_ended(data, handle);
		// TouchInnerHandle uses the contact's down-time surface and origin;
		// crossing a menu edge never transfers an existing contact.
		handle.motion(data, focus, event, seq);
	}

	fn frame(&mut self, data: &mut MoonshineCompositor, handle: &mut TouchInnerHandle<'_, MoonshineCompositor>, seq: Serial) {
		self.finish_if_ended(data, handle);
		handle.frame(data, seq);
	}

	fn cancel(&mut self, data: &mut MoonshineCompositor, handle: &mut TouchInnerHandle<'_, MoonshineCompositor>, seq: Serial) {
		self.finish_if_ended(data, handle);
		handle.cancel(data, seq);
	}

	fn shape(&mut self, data: &mut MoonshineCompositor, handle: &mut TouchInnerHandle<'_, MoonshineCompositor>, event: &ShapeEvent, seq: Serial) {
		self.finish_if_ended(data, handle);
		handle.shape(data, event, seq);
	}

	fn orientation(&mut self, data: &mut MoonshineCompositor, handle: &mut TouchInnerHandle<'_, MoonshineCompositor>, event: &OrientationEvent, seq: Serial) {
		self.finish_if_ended(data, handle);
		handle.orientation(data, event, seq);
	}

	fn start_data(&self) -> &GrabStartData<MoonshineCompositor> {
		&self.start_data
	}

	fn unset(&mut self, _data: &mut MoonshineCompositor) {
		// Replacing the adapter for a nested popup must not dismiss its
		// shared grab or cancel contacts already tracked by Smithay.
	}
}
