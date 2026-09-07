use std::time::Duration;

use super::*;
use crate::session::application::{Application, ApplicationConfig, ApplicationContext};

fn manager() -> SessionManager {
	SessionManager::new(
		Default::default(),
		Default::default(),
		Default::default(),
		Default::default(),
		"127.0.0.1".into(),
		10,
		false,
		ShutdownManager::new(),
	)
	.unwrap()
}

async fn install_process_owner(
	manager: &SessionManager,
) -> (Arc<SessionProcesses>, ShutdownManager<SessionShutdownReason>) {
	let mut guard = manager.inner.lock().await;
	let stop = guard.stop.clone();
	let processes = Arc::new(
		SessionProcesses::create(stop.clone(), guard.shutdown.clone())
			.await
			.unwrap(),
	);
	guard.processes = Some(processes.clone());
	spawn_session_watchdog(&manager.inner, &mut guard);
	(processes, stop)
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn cancelling_a_launch_cleans_up_while_the_session_state_is_absent() {
	let manager = manager();
	let (processes, stop) = install_process_owner(&manager).await;
	let group = processes.group();
	let dir = tempfile::tempdir().unwrap();
	let pre_started = dir.path().join("pre-started");
	let main_started = dir.path().join("main-started");
	let pre_script = format!("touch {}; sleep 30", pre_started.display());
	let config = ApplicationConfig {
		command: vec!["touch".into(), main_started.to_string_lossy().into_owned()],
		pre_command: vec![vec!["sh".into(), "-c".into(), pre_script]],
		launch_timeout_secs: 0,
		..Default::default()
	};
	let launch_owner = processes.clone();
	let launching = tokio::spawn(async move {
		let _cancellation = CleanupOnDrop::new(launch_owner.clone());
		Application::spawn(
			config,
			ApplicationContext {
				group: launch_owner.group(),
				pulse_socket_path: "/tmp/moonshine-test/pulse/native".into(),
				xdisplay: 0,
				wayland_display: "moonshine-test".into(),
				hdr: false,
				extra_env: Default::default(),
			},
			stop,
		)
		.await
	});
	tokio::time::timeout(Duration::from_secs(5), async {
		while !pre_started.exists() {
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.unwrap();
	assert!(manager.inner.lock().await.session.is_none());
	launching.abort();
	let _ = launching.await;
	manager.stop_session().await.unwrap();
	assert!(!group.populated().await.unwrap());
	assert!(
		!main_started.exists(),
		"The cancelled launch started the application afterwards"
	);
	assert!(manager.inner.lock().await.processes.is_none());
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn the_session_slot_stays_reserved_until_in_process_tasks_finish() {
	let manager = manager();
	let (processes, stop) = install_process_owner(&manager).await;
	let delay = stop.delay_shutdown_token().unwrap();
	let stopping_manager = manager.clone();
	let stopping = tokio::spawn(async move { stopping_manager.stop_session().await });
	processes.wait().await.unwrap();
	assert!(manager.inner.lock().await.processes.is_some());
	assert!(!stopping.is_finished());
	drop(delay);
	stopping.await.unwrap().unwrap();
	assert!(manager.inner.lock().await.processes.is_none());
}
