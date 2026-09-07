use std::time::Duration;

use async_shutdown::ShutdownManager;

use super::*;

#[test]
fn missing_runtime_executable_identifies_the_dependency() {
	let name = "moonshine-test-missing-runtime-executable";
	let error = runtime_executable(name).unwrap_err();
	assert_eq!(error.kind(), io::ErrorKind::NotFound);
	assert!(error.to_string().contains(name));
	assert!(error.to_string().contains("PATH"));
}

#[test]
fn cgroup_events_requires_an_explicit_population_value() {
	assert_eq!(parse_populated("populated 1\nfrozen 0\n"), Ok(true));
	assert_eq!(parse_populated("frozen 0\npopulated 0\n"), Ok(false));
	assert!(parse_populated("frozen 0\n").is_err());
	assert!(parse_populated("populated invalid\n").is_err());
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn shutdown_kills_a_stubborn_process_and_its_detached_child() {
	let stop = ShutdownManager::new();
	let shutdown = ShutdownManager::new();
	let processes = SessionProcesses::create_with_policy(
		stop,
		shutdown,
		ShutdownPolicy {
			grace: Duration::from_millis(300),
			force: Duration::from_secs(5),
		},
	)
	.await
	.unwrap();
	let group = processes.group();
	let dir = tempfile::tempdir().unwrap();
	let ready = dir.path().join("ready");
	let child = format!("trap '' TERM; touch {}; while :; do sleep 1; done", ready.display());
	let script = format!(
		"trap '' TERM; setsid sh -c {} & wait",
		shlex::try_quote(&child).unwrap()
	);
	let status = tokio::process::Command::new("systemd-run")
		.args([
			"--user",
			"--quiet",
			"--collect",
			"--property=Type=exec",
			"--property=SendSIGKILL=no",
		])
		.arg(format!("--unit={}", group.application_unit))
		.arg(format!("--slice={}", group.slice))
		.arg(format!("--property=Requisite={}", group.gate_unit))
		.arg(format!("--property=After={}", group.gate_unit))
		.args(["--", "sh", "-c", &script])
		.status()
		.await
		.unwrap();
	assert!(status.success());
	wait_until(|| ready.exists()).await;
	assert!(group.populated().await.unwrap());
	let before = std::time::Instant::now();
	processes.shutdown(SessionShutdownReason::UserStopped).await.unwrap();
	assert!(
		before.elapsed() >= Duration::from_millis(300),
		"Stubborn processes were killed before the grace period"
	);
	assert!(!group.populated().await.unwrap());
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
	tokio::time::timeout(Duration::from_secs(10), async {
		while !condition() {
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
	})
	.await
	.expect("Timed out waiting for test process");
}

async fn application(
	processes: &SessionProcesses,
	stop: ShutdownManager<SessionShutdownReason>,
	script: String,
	post_command: Vec<Vec<String>>,
) -> super::super::application::Application {
	use super::super::application::{Application, ApplicationConfig, ApplicationContext};
	Application::spawn(
		ApplicationConfig {
			command: vec!["sh".into(), "-c".into(), script],
			post_command,
			launch_timeout_secs: 0,
			..Default::default()
		},
		ApplicationContext {
			group: processes.group(),
			pulse_socket_path: PathBuf::from("/tmp/moonshine-test/pulse/native"),
			xdisplay: 0,
			wayland_display: "moonshine-test".into(),
			hdr: false,
			extra_env: Default::default(),
		},
		stop,
	)
	.await
	.unwrap()
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn graceful_shutdown_runs_the_handler_and_post_command() {
	let stop = ShutdownManager::new();
	let processes = SessionProcesses::create(stop.clone(), ShutdownManager::new())
		.await
		.unwrap();
	let dir = tempfile::tempdir().unwrap();
	let ready = dir.path().join("ready");
	let handled = dir.path().join("handled");
	let post = dir.path().join("post");
	let script = format!(
		"trap 'touch {}; exit 0' TERM; touch {}; while :; do sleep 0.1; done",
		handled.display(),
		ready.display()
	);
	let _app = application(
		&processes,
		stop,
		script,
		vec![vec!["touch".into(), post.to_string_lossy().into_owned()]],
	)
	.await;
	wait_until(|| ready.exists()).await;
	processes.shutdown(SessionShutdownReason::UserStopped).await.unwrap();
	assert!(
		handled.exists(),
		"Application never received its graceful shutdown opportunity"
	);
	assert!(post.exists(), "Shutdown returned before the post command completed");
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn launcher_exit_does_not_kill_the_real_application() {
	let stop = ShutdownManager::new();
	let processes = SessionProcesses::create(stop.clone(), ShutdownManager::new())
		.await
		.unwrap();
	let _app = application(&processes, stop.clone(), "sleep 30 & exit 0".into(), vec![]).await;
	tokio::time::sleep(Duration::from_millis(150)).await;
	let running = processes.group().populated().await.unwrap();
	let stopped = stop.is_shutdown_triggered();
	processes.shutdown(SessionShutdownReason::UserStopped).await.unwrap();
	assert!(running, "The launcher exit terminated its child");
	assert!(!stopped, "The launcher exit was treated as session exit");
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn lost_daemon_lease_stops_only_its_session() {
	let stop = ShutdownManager::new();
	let first = SessionProcesses::create(stop.clone(), ShutdownManager::new())
		.await
		.unwrap();
	let second_stop = ShutdownManager::new();
	let second = SessionProcesses::create(second_stop.clone(), ShutdownManager::new())
		.await
		.unwrap();
	let _first_app = application(&first, stop, "sleep 30".into(), vec![]).await;
	let _second_app = application(&second, second_stop, "sleep 30".into(), vec![]).await;
	// Simulate the kernel closing the daemon's writer on process death. No
	// Rust shutdown request is sent: systemd must initiate cleanup itself.
	first.group.lease.shutdown(std::net::Shutdown::Both).unwrap();
	first.wait().await.unwrap();
	let second_alive = second.group().populated().await.unwrap();
	second.shutdown(SessionShutdownReason::UserStopped).await.unwrap();
	assert!(second_alive, "Stopping one session affected the other session");
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn process_shutdown_waits_for_the_compositor_to_close_xlib() {
	let stop = ShutdownManager::new();
	let processes = SessionProcesses::create(stop.clone(), ShutdownManager::new())
		.await
		.unwrap();
	let group = processes.group();
	let compositor = group.begin_compositor().unwrap();
	let _app = application(&processes, stop, "sleep 30".into(), vec![]).await;
	processes.request_stop(SessionShutdownReason::UserStopped);
	tokio::time::sleep(Duration::from_millis(100)).await;
	let still_alive = group.populated().await.unwrap();
	compositor.release_x11();
	processes.wait().await.unwrap();
	assert!(still_alive, "Processes were terminated before Xlib disconnected");
	assert!(!group.populated().await.unwrap());
	assert!(
		group.begin_compositor().is_err(),
		"A compositor started during teardown"
	);
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn xwayland_wrapper_preserves_fds_and_cannot_launch_after_shutdown() {
	use std::io::Read;
	use std::os::fd::AsRawFd;
	use std::os::unix::process::CommandExt;
	let processes = SessionProcesses::create(ShutdownManager::new(), ShutdownManager::new())
		.await
		.unwrap();
	let group = processes.group();
	let fake = group.wrapper_dir.path().join("fake Xwayland's executable");
	let shell = which::which("sh").unwrap();
	std::fs::write(
		&fake,
		format!(
			"#!{}\nprintf '%s\\n' \"$1\"\ncat /proc/self/cgroup\nprintf inherited >&3\n",
			shell.display()
		),
	)
	.unwrap();
	std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
	group.write_xwayland_wrapper_for(&fake).unwrap();
	let (mut reader, writer) = UnixStream::pair().unwrap();
	reader.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
	let fd = writer.as_raw_fd();
	let literal = "literal $HOME ' ; ";
	let mut command = std::process::Command::new("Xwayland");
	command.env("PATH", group.xwayland_path().unwrap()).arg(literal);
	// Match Smithay's inherited-descriptor contract without launching a GPU.
	unsafe {
		command.pre_exec(move || {
			if libc::dup2(fd, 3) < 0 {
				return Err(io::Error::last_os_error());
			}
			if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
				return Err(io::Error::last_os_error());
			}
			Ok(())
		});
	}
	let output = command.output().unwrap();
	drop(writer);
	let mut received = String::new();
	reader.read_to_string(&mut received).unwrap();
	processes.shutdown(SessionShutdownReason::UserStopped).await.unwrap();
	let late = tokio::process::Command::new("Xwayland")
		.env("PATH", group.xwayland_path().unwrap())
		.arg(literal)
		.output()
		.await
		.unwrap();
	// A failed late scope may leave an empty, automatically loaded slice.
	let _ = group.queue_stop(&group.slice).await;
	assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
	assert_eq!(received, "inherited");
	let stdout = String::from_utf8(output.stdout).unwrap();
	assert!(stdout.starts_with(literal));
	assert!(stdout.contains(&group.slice));
	assert!(stdout.contains(&group.xwayland_unit));
	assert!(!late.status.success(), "A stopped session was resurrected");
}

#[test]
fn cgroup_paths_cannot_resolve_to_the_root_or_escape_it() {
	assert!(cgroup_path("/").is_err());
	assert!(cgroup_path("/../system.slice").is_err());
	assert!(cgroup_path("system.slice").is_err());
	assert_eq!(
		cgroup_path("/user.slice/example.slice").unwrap(),
		PathBuf::from("/sys/fs/cgroup/user.slice/example.slice")
	);
}

#[tokio::test]
#[ignore = "requires a user systemd manager and cgroup v2"]
async fn cancelling_the_shutdown_waiter_does_not_cancel_cleanup() {
	let processes = SessionProcesses::create(ShutdownManager::new(), ShutdownManager::new())
		.await
		.unwrap();
	let group = processes.group();
	processes.request_stop(SessionShutdownReason::UserStopped);
	let finished = processes.finished.clone();
	drop(processes);
	wait_for_shutdown(finished).await.unwrap();
	assert!(!group.populated().await.unwrap());
}
