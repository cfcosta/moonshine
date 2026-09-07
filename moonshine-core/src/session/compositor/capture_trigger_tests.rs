//! Real protocol commits and renderer visibility drive the readiness decision.

use super::*;
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::test::{DummyFramebuffer, DummyRenderer};
use smithay::desktop::space::{SpaceRenderElements, space_render_elements};

fn scene(h: &mut Harness) -> RenderElementStates {
	let mut renderer = DummyRenderer;
	let elements = if h.state.is_override_active() {
		let popups = h.state.popup_surfaces_for_render();
		super::super::render_scene::override_render_elements(
			&mut renderer,
			&h.state.override_surface.as_ref().unwrap().0,
			&popups,
		)
		.into_iter()
		.map(SpaceRenderElements::Surface)
		.collect()
	} else {
		space_render_elements(&mut renderer, [&h.state.space], &h.state.output, 1.0).unwrap()
	};
	let mut probe = smithay::backend::renderer::damage::OutputDamageTracker::from_output(&h.state.output);
	let (_, visible) = probe.damage_output(0, &elements).unwrap();
	let rendered = h
		.state
		.damage_tracker
		.render_output(&mut renderer, &mut DummyFramebuffer, 0, &elements, [0.0, 0.0, 0.0, 1.0])
		.unwrap()
		.states;
	for id in visible.states.keys().chain(rendered.states.keys()) {
		assert_eq!(
			visible.element_was_presented(id.clone()),
			rendered.element_was_presented(id.clone())
		);
	}
	visible
}

fn accept(h: &mut Harness) -> bool {
	let scene = scene(h);
	h.state.capture_trigger.accepted(&scene)
}

fn damage(h: &mut Harness, surface: &wl_surface::WlSurface) {
	surface.damage_buffer(0, 0, 20, 20);
	surface.commit();
	h.roundtrip();
}

#[test]
fn callback_only_and_sync_requests_do_not_signal_new_content() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	assert!(accept(&mut h));
	root.surface.frame(&h.qh, 91);
	root.surface.commit();
	h.roundtrip();
	assert!(!accept(&mut h));
	h.roundtrip();
	assert!(!accept(&mut h));
	damage(&mut h, &root.surface);
	assert!(accept(&mut h), "damage without a new attachment still changes content");
	assert!(!accept(&mut h), "accepted damage must not be replayed");
}

#[test]
fn reattaching_the_same_buffer_is_new_content() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	assert!(accept(&mut h));
	root.surface.attach(Some(h.buffers.last().unwrap()), 0, 0);
	root.surface.commit();
	h.roundtrip();
	assert!(accept(&mut h));
}

#[test]
fn synchronized_children_wait_for_the_parent_and_desynchronized_children_do_not() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	assert!(accept(&mut h));
	let child = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	let sub = h
		.client
		.subcompositor
		.as_ref()
		.unwrap()
		.get_subsurface(&child, &root.surface, &h.qh, ());
	h.map(&child, 80, 60);
	assert!(!accept(&mut h), "cached sync-child commit is not applied");
	root.surface.commit();
	h.roundtrip();
	assert!(accept(&mut h), "parent latch must find the child's attachment");
	damage(&mut h, &child);
	assert!(!accept(&mut h));
	root.surface.commit();
	h.roundtrip();
	assert!(accept(&mut h), "parent latch must find child damage too");
	sub.set_desync();
	damage(&mut h, &child);
	assert!(accept(&mut h));
}

#[test]
fn failed_capture_queries_keep_readiness_and_multiple_commits_coalesce() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	damage(&mut h, &root.surface);
	damage(&mut h, &root.surface);
	let scene = scene(&mut h);
	assert!(h.state.capture_trigger.ready_for(&scene));
	assert!(h.state.capture_trigger.ready_for(&scene));
	assert!(h.state.capture_trigger.accepted(&scene));
	assert!(!h.state.capture_trigger.ready_for(&scene));
}

#[test]
fn unmapped_and_offscreen_content_cannot_trigger_capture() {
	let mut h = Harness::new();
	let hidden = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	h.map(&hidden, 80, 60);
	assert!(!accept(&mut h));
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let window = h.state.space.elements().next().unwrap().clone();
	h.state.space.map_element(window, (1000, 1000), false);
	assert!(!accept(&mut h));
}

#[test]
fn fully_occluded_content_is_ignored_but_partial_visibility_counts() {
	let mut h = Harness::new();
	let back = h.toplevel();
	h.map(&back.surface, 800, 600);
	let front = h.toplevel();
	let opaque = h.client.compositor.as_ref().unwrap().create_region(&h.qh, ());
	opaque.add(0, 0, 800, 600);
	front.surface.set_opaque_region(Some(&opaque));
	h.map(&front.surface, 800, 600);
	assert!(accept(&mut h));
	damage(&mut h, &back.surface);
	assert!(!accept(&mut h));
	let front_server = h.server_surface(&front.surface);
	let front_window = h
		.state
		.space
		.elements()
		.find(|w| w.toplevel().unwrap().wl_surface() == &front_server)
		.unwrap()
		.clone();
	h.state.space.map_element(front_window, (100, 0), false);
	damage(&mut h, &back.surface);
	assert!(accept(&mut h));
}

#[test]
fn popup_unmap_and_destroy_repaint_previously_visible_content() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let popup = h.popup(&root.xdg_surface, (40, 50, 100, 80));
	h.map(&popup.surface, 100, 80);
	assert!(accept(&mut h));
	popup.surface.attach(None, 0, 0);
	popup.surface.commit();
	h.roundtrip();
	assert!(accept(&mut h), "unmapping changes the previously captured scene");
	let popup = h.popup(&root.xdg_surface, (40, 50, 100, 80));
	h.map(&popup.surface, 100, 80);
	assert!(accept(&mut h));
	popup.popup.destroy();
	popup.xdg_surface.destroy();
	popup.surface.destroy();
	h.roundtrip();
	assert!(accept(&mut h));
	assert!(!accept(&mut h));
}

#[test]
fn active_override_ignores_replaced_base_but_keeps_popup_updates() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let popup = h.popup(&root.xdg_surface, (40, 50, 100, 80));
	h.map(&popup.surface, 100, 80);
	let replacement = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	h.map(&replacement, 800, 600);
	h.state.override_surface = Some((h.server_surface(&replacement), 0));
	assert!(accept(&mut h));
	damage(&mut h, &root.surface);
	assert!(!accept(&mut h));
	damage(&mut h, &popup.surface);
	assert!(accept(&mut h));
	damage(&mut h, &replacement);
	assert!(accept(&mut h));
}

#[test]
fn inactive_override_and_unselected_direct_buffer_do_not_signal_readiness() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let root_server = h.server_surface(&root.surface);
	let hidden = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	h.map(&hidden, 800, 600);
	h.state.override_surface = Some((h.server_surface(&hidden), 42));
	assert!(accept(&mut h));
	damage(&mut h, &hidden);
	assert!(!accept(&mut h));
	damage(&mut h, &hidden);
	assert!(!h.state.capture_trigger.accepted_direct(&root_server));
	damage(&mut h, &root.surface);
	assert!(h.state.capture_trigger.ready_for_direct(&root_server));
	assert!(h.state.capture_trigger.ready_for_direct(&root_server));
	assert!(h.state.capture_trigger.accepted_direct(&root_server));
	assert!(!h.state.capture_trigger.ready_for_direct(&root_server));
}

#[test]
fn pacing_releases_clients_before_capture_and_does_not_claim_presentation() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
	h.map(&menu.surface, 120, 100);
	let frame = h.request_frame(&root.surface);
	let menu_frame = h.request_frame(&menu.surface);
	let presentation = h.request_presentation(&root.surface);
	h.roundtrip();

	assert!(!h.state.capture_available);
	h.state.frame_tick(); // First tick releases the client, without capturing its old buffer.
	h.roundtrip();
	assert!(h.state.capture_available);
	assert!(h.frame_done(frame) && h.frame_done(menu_frame));
	assert!(!h.presentation_received(presentation));
	assert_eq!(h.state.render_count, 0);

	// Even with pending visible content, callback-only commits cannot spend the
	// opportunity before the client has submitted its response to this tick.
	let revision = h.state.capture_trigger.revision();
	h.request_frame(&root.surface);
	h.roundtrip();
	h.state.capture_after_dispatch(revision);
	assert!(h.state.capture_available);
	assert_eq!(h.state.render_count, 0);
}

#[test]
fn exhausted_capture_opportunity_coalesces_late_commits_until_next_tick() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	assert!(accept(&mut h));
	let revision = h.state.capture_trigger.revision();
	// Model a used opportunity, including an export rejected under backpressure.
	h.state.capture_available = false;
	damage(&mut h, &root.surface);
	h.state.capture_after_dispatch(revision);
	let states = scene(&mut h);
	assert!(h.state.capture_trigger.ready_for(&states));
	assert!(!h.state.capture_available);
	let frame = h.request_frame(&root.surface);
	h.roundtrip();
	h.state.frame_tick();
	h.roundtrip();
	assert!(h.frame_done(frame), "a rejected export must not stall client pacing");
	assert!(h.state.capture_available);
	assert!(
		h.state.capture_trigger.ready_for(&states),
		"pacing must retain unaccepted content"
	);
}

#[test]
fn applied_subsurface_buffer_prevents_single_buffer_capture() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let server_root = h.server_surface(&root.surface);
	let child = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	let _sub = h
		.client
		.subcompositor
		.as_ref()
		.unwrap()
		.get_subsurface(&child, &root.surface, &h.qh, ());
	h.map(&child, 80, 60);
	assert!(super::super::render_scene::single_buffer_tree(&server_root));
	root.surface.commit();
	h.roundtrip();
	assert!(!super::super::render_scene::single_buffer_tree(&server_root));
	child.attach(None, 0, 0);
	child.commit();
	h.roundtrip();
	assert!(!super::super::render_scene::single_buffer_tree(&server_root));
	root.surface.commit();
	h.roundtrip();
	assert!(super::super::render_scene::single_buffer_tree(&server_root));
}

#[test]
fn override_pacing_survives_popup_and_focus_transitions_without_capture() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let replacement = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	h.map(&replacement, 800, 600);
	h.state.override_surface = Some((h.server_surface(&replacement), 0));
	let menu = h.popup(&root.xdg_surface, (40, 50, 120, 100));
	h.map(&menu.surface, 120, 100);
	let frame = h.request_frame(&replacement);
	let menu_frame = h.request_frame(&menu.surface);
	let presentation = h.request_presentation(&replacement);
	h.roundtrip();
	assert!(!h.state.can_direct_scanout_scene());
	h.state.frame_tick();
	h.roundtrip();
	assert!(h.frame_done(frame) && h.frame_done(menu_frame));
	assert!(!h.presentation_received(presentation));

	let hidden_frame = h.request_frame(&replacement);
	h.roundtrip();
	h.state.focused_x11_window = Some(123);
	h.state.send_frame_callbacks();
	h.roundtrip();
	assert!(
		!h.frame_done(hidden_frame),
		"inactive replacement must not receive pacing callbacks"
	);
	h.state.focused_x11_window = None;
	menu.surface.attach(None, 0, 0);
	menu.surface.commit();
	h.roundtrip();
	assert!(h.state.can_direct_scanout_scene());
	h.state.send_frame_callbacks();
	h.roundtrip();
	assert!(
		h.frame_done(hidden_frame),
		"reactivated replacement must resume even without capture"
	);
}

#[test]
fn unrelated_dispatch_cannot_spend_a_slot_on_older_visible_content() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	assert!(accept(&mut h));
	damage(&mut h, &root.surface); // Arrived while the previous opportunity was spent.
	let revision = h.state.capture_trigger.revision();
	let hidden = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	h.map(&hidden, 80, 60); // Unrelated content wakes the next interval's dispatch.
	let states = scene(&mut h);
	let root_surface = h.server_surface(&root.surface);
	assert!(h.state.capture_trigger.revision() > revision);
	assert!(
		h.state.capture_trigger.ready_for(&states),
		"older visible content remains pending"
	);
	assert!(!h.state.capture_trigger.ready_for_since(&states, revision));
	assert!(!h.state.capture_trigger.ready_for_direct_since(&root_surface, revision));
	damage(&mut h, &root.surface);
	assert!(h.state.capture_trigger.ready_for_since(&states, revision));
	assert!(h.state.capture_trigger.ready_for_direct_since(&root_surface, revision));
}

#[test]
fn destroyed_visible_subsurface_invalidates_even_when_capture_slot_is_spent() {
	let mut h = Harness::new();
	let root = h.toplevel();
	h.map(&root.surface, 800, 600);
	let child = h.client.compositor.as_ref().unwrap().create_surface(&h.qh, ());
	let _sub = h
		.client
		.subcompositor
		.as_ref()
		.unwrap()
		.get_subsurface(&child, &root.surface, &h.qh, ());
	h.map(&child, 80, 60);
	root.surface.commit();
	h.roundtrip();
	assert!(accept(&mut h));
	h.state.screen_dirty = false;
	h.state.capture_available = false;
	child.destroy();
	h.roundtrip();
	assert!(h.state.screen_dirty, "fallback must repaint a destroyed visible child");
	let states = scene(&mut h);
	assert!(h.state.capture_trigger.ready_for(&states));
}
