//! Ownership and shutdown of the external processes belonging to one session.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_shutdown::ShutdownManager;
use futures_util::StreamExt;
use tokio::sync::{OnceCell, oneshot, watch};
use zbus::{Connection, Proxy};
use zvariant::{OwnedObjectPath, Value};

use super::manager::SessionShutdownReason;
use crate::ShutdownReason;

const BUS: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER: &str = "org.freedesktop.systemd1.Manager";
const SLICE: &str = "org.freedesktop.systemd1.Slice";
const BUS_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy)]
struct ShutdownPolicy {
	grace: Duration,
	force: Duration,
}

impl Default for ShutdownPolicy {
	fn default() -> Self {
		// Five seconds for the app, then two for Xwayland, plus job processing.
		Self {
			grace: Duration::from_secs(8),
			force: Duration::from_secs(5),
		}
	}
}

/// The session manager owns this handle even while its session state is being
/// launched. A detached supervisor owns cleanup, so cancelling a caller's
/// future cannot cancel termination. Drop requests cleanup as a fallback.
pub(super) struct SessionProcesses {
	group: Arc<ProcessGroup>,
	request: watch::Sender<Option<SessionShutdownReason>>,
	finished: watch::Receiver<Option<Result<(), ()>>>,
}

/// Launch context only. Cloning this does not transfer lifecycle ownership.
pub(crate) struct ProcessGroup {
	conn: Connection,
	pub(super) slice: String,
	pub(super) gate_unit: String,
	pub(super) application_unit: String,
	pub(super) xwayland_unit: String,
	guardian_unit: String,
	lease: UnixStream,
	cgroup: OnceCell<PathBuf>,
	wrapper_dir: tempfile::TempDir,
	stopping: AtomicBool,
	compositor_release: std::sync::Mutex<Option<watch::Receiver<bool>>>,
}

/// Acknowledges that Xlib is closed before Xwayland can be terminated. If the
/// compositor returns early, its local state drops before this argument does.
pub(crate) struct CompositorProcessGuard {
	pub(crate) group: Arc<ProcessGroup>,
	released: watch::Sender<bool>,
}

impl CompositorProcessGuard {
	pub(crate) fn release_x11(&self) {
		self.released.send_replace(true);
	}
}

impl Drop for CompositorProcessGuard {
	fn drop(&mut self) {
		self.release_x11();
	}
}

impl SessionProcesses {
	pub(super) async fn create(
		stop: ShutdownManager<SessionShutdownReason>,
		shutdown: ShutdownManager<ShutdownReason>,
	) -> Result<Self, ()> {
		Self::create_with_policy(stop, shutdown, ShutdownPolicy::default()).await
	}

	async fn create_with_policy(
		stop: ShutdownManager<SessionShutdownReason>,
		shutdown: ShutdownManager<ShutdownReason>,
		policy: ShutdownPolicy,
	) -> Result<Self, ()> {
		let conn = zbus::connection::Builder::session()
			.map_err(log_bus_error)?
			.method_timeout(BUS_TIMEOUT)
			.build()
			.await
			.map_err(|e| tracing::error!("Session bus: {e}"))?;
		let delay = shutdown.delay_shutdown_token().map_err(|_| ())?;
		let id = uuid::Uuid::new_v4().simple().to_string();
		let (lease, guardian_input) = UnixStream::pair().map_err(|e| tracing::error!("Session liveness pipe: {e}"))?;
		let group = Arc::new(ProcessGroup {
			conn,
			slice: format!("moonshine-session-{id}.slice"),
			gate_unit: format!("moonshine-gate-{id}.service"),
			application_unit: format!("moonshine-app-{id}.service"),
			xwayland_unit: format!("moonshine-xwayland-{id}.scope"),
			guardian_unit: format!("moonshine-guardian-{id}.service"),
			lease,
			cgroup: OnceCell::new(),
			wrapper_dir: tempfile::Builder::new()
				.prefix("moonshine-xwayland-")
				.tempdir()
				.map_err(|e| tracing::error!("Create Xwayland launcher directory: {e}"))?,
			stopping: AtomicBool::new(false),
			compositor_release: std::sync::Mutex::new(None),
		});
		group
			.write_xwayland_wrapper()
			.map_err(|e| tracing::error!("Prepare Xwayland launcher: {e}"))?;
		let (request, mut requests) = watch::channel(None);
		let (finished_tx, finished) = watch::channel(None);
		let (ready_tx, ready) = oneshot::channel();
		let owner = Self {
			group: group.clone(),
			request,
			finished,
		};

		// Install the supervisor BEFORE the first StartTransientUnit call.
		tokio::spawn(async move {
			let _delay = delay;
			let initialized = group.initialize(guardian_input).await;
			let accepted = ready_tx.send(initialized).is_ok();
			let reason = if initialized.is_err() || !accepted {
				SessionShutdownReason::UserStopped
			} else {
				tokio::select! {
					reason = async {
						requests.wait_for(|reason| reason.is_some()).await
							.ok().and_then(|reason| *reason).unwrap_or(SessionShutdownReason::UserStopped)
					} => reason,
					reason = stop.wait_shutdown_triggered() => reason,
					_ = shutdown.wait_shutdown_triggered() => SessionShutdownReason::ManagerShutdown,
				}
			};
			group.stopping.store(true, Ordering::Release);
			tracing::info!(slice = group.slice, ?reason, "Stopping session process subtree.");
			let result = group.terminate(policy).await;
			let _ = stop.trigger_shutdown(reason);
			if result.is_err() {
				tracing::error!(
					slice = group.slice,
					"Session process cleanup failed; shutting down server."
				);
				let _ = shutdown.trigger_shutdown(ShutdownReason::SessionManagerShutdown);
			}
			finished_tx.send_replace(Some(result));
		});

		if ready.await.map_err(|_| ())?.is_err() {
			let _ = owner.wait().await;
			return Err(());
		}
		Ok(owner)
	}

	pub(super) fn group(&self) -> Arc<ProcessGroup> {
		self.group.clone()
	}

	pub(super) fn request_stop(&self, reason: SessionShutdownReason) {
		self.group.stopping.store(true, Ordering::Release);
		self.request.send_if_modified(|current| {
			if current.is_some() {
				return false;
			}
			*current = Some(reason);
			true
		});
	}

	pub(super) async fn shutdown(&self, reason: SessionShutdownReason) -> Result<(), ()> {
		self.request_stop(reason);
		self.wait().await
	}

	pub(super) async fn wait(&self) -> Result<(), ()> {
		wait_for_shutdown(self.finished.clone()).await
	}
}

impl Drop for SessionProcesses {
	fn drop(&mut self) {
		self.request_stop(SessionShutdownReason::UserStopped);
	}
}

async fn wait_for_shutdown(mut finished: watch::Receiver<Option<Result<(), ()>>>) -> Result<(), ()> {
	let result = *finished.wait_for(|result| result.is_some()).await.map_err(|_| ())?;
	result.unwrap_or(Err(()))
}

impl ProcessGroup {
	pub(crate) fn begin_compositor(self: &Arc<Self>) -> Result<CompositorProcessGuard, ()> {
		let mut registration = self.compositor_release.lock().map_err(|_| ())?;
		if self.is_stopping() || registration.is_some() {
			return Err(());
		}
		let (released, receiver) = watch::channel(false);
		*registration = Some(receiver);
		Ok(CompositorProcessGuard {
			group: self.clone(),
			released,
		})
	}

	async fn wait_for_x11_release(&self) -> Result<(), ()> {
		let receiver = self.compositor_release.lock().map_err(|_| ())?.clone();
		if let Some(mut receiver) = receiver {
			tokio::time::timeout(BUS_TIMEOUT, receiver.wait_for(|released| *released))
				.await
				.map_err(|_| tracing::error!("Compositor did not release Xlib before process shutdown."))?
				.map_err(|_| ())?;
		}
		Ok(())
	}

	pub(super) fn is_stopping(&self) -> bool {
		self.stopping.load(Ordering::Acquire)
	}

	pub(crate) fn xwayland_path(&self) -> Result<std::ffi::OsString, ()> {
		let mut paths = vec![self.wrapper_dir.path().to_path_buf()];
		paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
		std::env::join_paths(paths).map_err(|e| tracing::error!("Xwayland launcher PATH: {e}"))
	}

	fn write_xwayland_wrapper(&self) -> io::Result<()> {
		self.write_xwayland_wrapper_for(&which::which("Xwayland").map_err(io::Error::other)?)
	}

	fn write_xwayland_wrapper_for(&self, xwayland: &Path) -> io::Result<()> {
		let resolve = |name| which::which(name).map_err(io::Error::other);
		let shell = resolve("sh")?;
		let runner = resolve("systemd-run")?;
		let args = [
			runner.to_string_lossy().into_owned(),
			"--user".into(),
			"--scope".into(),
			"--quiet".into(),
			"--no-ask-password".into(),
			"--collect".into(),
			"--expand-environment=no".into(),
			format!("--unit={}", self.xwayland_unit),
			format!("--slice={}", self.slice),
			format!("--property=Requisite={}", self.gate_unit),
			format!("--property=After={}", self.gate_unit),
			"--property=KillMode=control-group".into(),
			"--property=TimeoutStopSec=2s".into(),
			"--property=SendSIGKILL=yes".into(),
			"--".into(),
			xwayland.to_string_lossy().into_owned(),
		];
		let command = shlex::try_join(args.iter().map(String::as_str)).map_err(io::Error::other)?;
		let script = format!("#!{}\nexec {command} \"$@\"\n", shell.display());
		let path = self.wrapper_dir.path().join("Xwayland");
		std::fs::write(&path, script)?;
		std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
	}

	async fn initialize(&self, guardian_input: UnixStream) -> Result<(), ()> {
		let proxy = self.manager().await?;
		proxy.call::<_, _, ()>("Subscribe", &()).await.map_err(log_bus_error)?;
		// EOF survives daemon crashes: systemd owns the reader, while only this
		// daemon holds the CLOEXEC writer. When it closes, the guardian exits and
		// BindsTo stops the entire slice, using systemd's own kill deadlines.
		let cat = which::which("cat").map_err(|e| tracing::error!("Find session guardian reader: {e}"))?;
		let cat = cat.to_string_lossy().into_owned();
		self.start_unit(
			&self.guardian_unit,
			vec![
				("Description", Value::from("Moonshine session liveness guardian")),
				("Type", Value::from("exec")),
				("Slice", Value::from("moonshine.slice")),
				("ExecStart", Value::from(vec![(cat.clone(), vec![cat], false)])),
				(
					"StandardInputFileDescriptor",
					Value::from(zvariant::Fd::from(&guardian_input)),
				),
				("StandardOutput", Value::from("null")),
				("StandardError", Value::from("journal")),
				("CollectMode", Value::from("inactive-or-failed")),
			],
		)
		.await?;
		self.start_unit(
			&self.slice,
			vec![
				("Description", Value::from("Moonshine session processes")),
				("BindsTo", Value::from(vec![self.guardian_unit.as_str()])),
				("After", Value::from(vec![self.guardian_unit.as_str()])),
			],
		)
		.await?;
		let path = self.unit_path(&self.slice).await?.ok_or(())?;
		let unit = Proxy::new(&self.conn, BUS, path, SLICE).await.map_err(log_bus_error)?;
		let cgroup: String = unit.get_property("ControlGroup").await.map_err(log_bus_error)?;
		let path = cgroup_path(&cgroup)?;
		self.cgroup.set(path).map_err(|_| ())?;
		// Requisite gates late scope/service starts without resurrecting a stopped
		// session. Stopping this gate propagates to both units; their After=
		// ordering keeps Xwayland alive while the application stops.
		let ready = which::which("true")
			.map_err(|e| tracing::error!("Find session gate command: {e}"))?
			.to_string_lossy()
			.into_owned();
		self.start_unit(
			&self.gate_unit,
			vec![
				("Description", Value::from("Moonshine session launch gate")),
				("Type", Value::from("oneshot")),
				("RemainAfterExit", Value::from(true)),
				("Slice", Value::from(self.slice.as_str())),
				("ExecStart", Value::from(vec![(ready.clone(), vec![ready], false)])),
				("BindsTo", Value::from(vec![self.slice.as_str()])),
				("After", Value::from(vec![self.slice.as_str()])),
			],
		)
		.await
	}

	async fn manager(&self) -> Result<Proxy<'_>, ()> {
		Proxy::new(&self.conn, BUS, MANAGER_PATH, MANAGER)
			.await
			.map_err(log_bus_error)
	}

	async fn start_unit(&self, name: &str, properties: Vec<(&str, Value<'_>)>) -> Result<(), ()> {
		let proxy = self.manager().await?;
		let mut jobs = proxy.receive_signal("JobRemoved").await.map_err(log_bus_error)?;
		let aux: Vec<(&str, Vec<(&str, Value<'_>)>)> = vec![];
		let job: OwnedObjectPath = proxy
			.call("StartTransientUnit", &(name, "fail", properties, aux))
			.await
			.map_err(log_bus_error)?;
		wait_job(&mut jobs, &job, BUS_TIMEOUT).await
	}

	async fn unit_path(&self, name: &str) -> Result<Option<OwnedObjectPath>, ()> {
		match self.manager().await?.call("GetUnit", &(name,)).await {
			Ok(path) => Ok(Some(path)),
			Err(e) if no_such_unit(&e) => Ok(None),
			Err(e) => {
				log_bus_error(e);
				Err(())
			},
		}
	}

	async fn queue_stop(&self, name: &str) -> Result<Option<OwnedObjectPath>, ()> {
		match self.manager().await?.call("StopUnit", &(name, "replace")).await {
			Ok(job) => Ok(Some(job)),
			Err(e) if no_such_unit(&e) => Ok(None),
			Err(e) => {
				log_bus_error(e);
				Err(())
			},
		}
	}

	async fn force_kill(&self) -> Result<(), ()> {
		match self
			.manager()
			.await?
			.call::<_, _, ()>("KillUnit", &(&self.slice, "all", libc::SIGKILL))
			.await
		{
			Ok(()) => Ok(()),
			Err(e) if no_such_unit(&e) => Ok(()),
			Err(e) => {
				log_bus_error(e);
				Err(())
			},
		}
	}

	pub(super) async fn populated(&self) -> Result<bool, ()> {
		let Some(path) = self.cgroup.get() else {
			return Ok(false);
		};
		match tokio::fs::read_to_string(path.join("cgroup.events")).await {
			Ok(contents) => parse_populated(&contents),
			Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
			Err(e) => {
				tracing::error!("Read session cgroup population: {e}");
				Err(())
			},
		}
	}

	async fn terminate(&self, policy: ShutdownPolicy) -> Result<(), ()> {
		let result = self.terminate_subtree(policy).await;
		// Even if D-Bus or population verification fails, close the liveness
		// writer so systemd's independent fallback can finish the cleanup.
		let _ = self.lease.shutdown(std::net::Shutdown::Both);
		let proxy = self.manager().await?;
		let mut jobs = proxy.receive_signal("JobRemoved").await.map_err(log_bus_error)?;
		if let Some(job) = self.queue_stop(&self.guardian_unit).await? {
			wait_job(&mut jobs, &job, BUS_TIMEOUT).await?;
		}
		result
	}

	async fn terminate_subtree(&self, policy: ShutdownPolicy) -> Result<(), ()> {
		// Xlib's default I/O error handler exits the whole process if Xwayland
		// disappears. Close that connection on its owning compositor thread first.
		self.wait_for_x11_release().await?;
		let proxy = self.manager().await?;
		let mut jobs = proxy.receive_signal("JobRemoved").await.map_err(log_bus_error)?;
		// Stopping the slice closes its launch gate and stops every nested unit
		// in one ordered systemd transaction (app before Xwayland).
		let stop_job = self.queue_stop(&self.slice).await?;
		let graceful = tokio::time::timeout(policy.grace, async {
			if let Some(job) = stop_job {
				wait_job(&mut jobs, &job, policy.grace).await?;
			}
			while self.populated().await? {
				tokio::time::sleep(POLL_INTERVAL).await;
			}
			Ok::<(), ()>(())
		})
		.await
		.is_ok_and(|result| result.is_ok());

		// A completed stop job alone is insufficient. Check the whole subtree,
		// including nested units, before declaring shutdown successful.
		if !graceful || self.populated().await? {
			tracing::warn!(
				slice = self.slice,
				"Graceful shutdown expired; killing session subtree."
			);
			self.force_kill().await?;
		}

		// Stop the slice too, covering any additional nested units. Do not report
		// success until its job (including post commands) AND its processes end.
		let job = self.queue_stop(&self.slice).await?;
		let settle = async {
			if let Some(job) = job {
				let wait = wait_job(&mut jobs, &job, policy.force);
				tokio::pin!(wait);
				loop {
					tokio::select! {
						result = &mut wait => { result?; break; },
						_ = tokio::time::sleep(POLL_INTERVAL) => {
							if self.populated().await? { self.force_kill().await?; }
						},
					}
				}
			}
			while self.populated().await? {
				self.force_kill().await?;
				tokio::time::sleep(POLL_INTERVAL).await;
			}
			Ok(())
		};
		tokio::time::timeout(policy.force, settle).await.map_err(|_| {
			tracing::error!(
				slice = self.slice,
				"Session subtree did not become empty after SIGKILL."
			);
		})?
	}
}

fn cgroup_path(path: &str) -> Result<PathBuf, ()> {
	let relative = path.strip_prefix('/').filter(|p| !p.is_empty()).ok_or(())?;
	if !Path::new(relative)
		.components()
		.all(|c| matches!(c, std::path::Component::Normal(_)))
	{
		return Err(());
	}
	Ok(Path::new("/sys/fs/cgroup").join(relative))
}

fn parse_populated(contents: &str) -> Result<bool, ()> {
	match contents.lines().find_map(|line| line.strip_prefix("populated ")) {
		Some("0") => Ok(false),
		Some("1") => Ok(true),
		_ => Err(()),
	}
}

fn no_such_unit(error: &zbus::Error) -> bool {
	matches!(error, zbus::Error::MethodError(name, ..) if name.as_str() == "org.freedesktop.systemd1.NoSuchUnit")
}

fn log_bus_error(error: zbus::Error) {
	#[cfg(test)]
	eprintln!("Session process systemd operation failed: {error}");
	tracing::error!("Session process systemd operation failed: {error}");
}

async fn wait_job(
	jobs: &mut zbus::proxy::SignalStream<'_>,
	job: &OwnedObjectPath,
	timeout: Duration,
) -> Result<(), ()> {
	tokio::time::timeout(timeout, async {
		while let Some(message) = jobs.next().await {
			let (_, path, _, result): (u32, OwnedObjectPath, String, String) =
				message.body().deserialize().map_err(|_| ())?;
			if &path == job {
				return if result == "done" { Ok(()) } else { Err(()) };
			}
		}
		Err(())
	})
	.await
	.map_err(|_| ())?
}

#[cfg(test)]
mod tests;
