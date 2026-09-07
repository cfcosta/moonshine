# Session process ownership

Each launch owns a unique systemd slice. Moonshine uses the user service manager
for all cgroup operations, including forced termination. The daemon and its
compositor/streaming threads remain outside that slice.

```text
moonshine.slice
├── moonshine-guardian-<id>.service  (reads a daemon-owned liveness pipe)
└── moonshine-session.slice
    └── moonshine-session-<id>.slice
        ├── moonshine-gate-<id>.service  (active, no resident process)
        ├── moonshine-xwayland-<id>.scope
        └── moonshine-app-<id>.service
            └── application descendants
```

## Design choices

| Approach | Assessment |
| --- | --- |
| Track PIDs or Unix process groups | Cannot reliably contain daemonizing children or children calling `setsid`. |
| Create and manage raw cgroups | Requires delegation and additional deployment permissions; duplicates systemd's job and process management. |
| Move processes after launching them | Introduces a window in which children can be created outside the session group. |
| Move the entire session into a worker process | Gives a strong process boundary, but requires a new IPC architecture for stream control, diagnostics, configuration, and session keys. |
| A systemd slice with an application service and Xwayland scope | Preserves existing application hooks and Smithay's descriptor contract; supports ordered shutdown, recursive termination, and observable units. Selected. |

Systemd owns its cgroup tree. Its delegation documentation explicitly separates
services/scopes from slices and discourages multiple managers writing the same
subtree. This implementation queries the slice's `ControlGroup` property instead
of guessing its filesystem path. It reads `cgroup.events`, but writes through
systemd's D-Bus API only.

## Launch

The process supervisor is installed before the first unit-creation request. A
cancelled initialization still has an owner responsible for cleanup. The session
manager retains that owner while the initialized/launched state is temporarily
moved into an asynchronous operation.

The guardian is a transient service running `cat` with a socket supplied as its
standard input. The daemon holds the other end with close-on-exec set. The slice
has `BindsTo` and `After` dependencies on the guardian. EOF, including daemon
death, makes the guardian exit and systemd stop the slice independently of Rust
destructors or the Tokio runtime.

An inert oneshot service with `RemainAfterExit=yes` is the launch gate. Application
and Xwayland units have `Requisite` and `After` dependencies on it. A late launch
must find an active gate; it cannot start that gate itself. A service is used
because systemd does not support transient target units.

Smithay resolves `Xwayland` using the environment passed to its spawn call. A
private temporary directory supplies a wrapper which executes
`systemd-run --user --scope` and then the real, previously resolved Xwayland binary.
The runner joins its scope before executing Xwayland, preserving the inherited
Wayland, X11 WM, display-ready, and listening socket descriptors. Neither the
daemon's global environment nor the desktop PATH is changed.

Applications retain `ExecStartPre`, `ExecStart`, `ExecStopPost`, and their configured
output destinations. `ExitType=cgroup` keeps a forking launcher's application
alive until its remaining processes exit. `After=<xwayland scope>` also orders
application shutdown before Xwayland shutdown.

## Shutdown

1. Mark the session as stopping. Have the compositor close its Xlib focus-control
   connection and acknowledge that it will not reopen it. This must happen before
   Xwayland exits: Xlib's default I/O error handler terminates the whole daemon.
   Then stop the slice. Systemd's transaction closes
   the launch gate and stops every nested unit in dependency order.
2. Give the application five seconds to stop; keep Xwayland available during
   that phase. Xwayland has a two-second stop timeout. Post commands execute as
   part of the application service's stop job.
3. Allow an overall eight-second graceful window, including job processing.
   If the stop job has not completed or `cgroup.events` still says `populated 1`,
   call `KillUnit(slice, "all", SIGKILL)` for the entire subtree.
4. Allow five seconds for forced termination. Wait for the slice stop job and
   `populated 0` (or removal of the cgroup). Repeat forced termination while a
   pending stop job can still spawn post commands. A stop reply alone is not
   proof that processes are gone.
5. Close the guardian pipe, stop the in-process compositor/streams, and release
   the session slot only after they finish. Cleanup failure shuts down the server
   instead of accepting another session over an unverified cleanup.

The supervisor runs independently of HTTP futures and holds a server shutdown
delay token. Dropping a handle requests cleanup; explicit shutdown awaits it.
Systemd's per-unit SIGKILL fallback stays enabled so a daemon crash does not leave
termination dependent on the vanished supervisor. In that fallback, systemd's
individual service/scope deadlines apply rather than the Rust supervisor's
overall deadline.

## Boundaries

This is lifecycle containment, not a security sandbox. An application which
forwards work to an already-running desktop instance, or asks another service to
launch work outside this slice, has not created a session-owned descendant.
Single-instance launchers still need an application-specific policy.

The implementation requires systemd 254+ (`--expand-environment=no`) and cgroup v2.
Current systemd uses the kernel's `cgroup.kill` operation for SIGKILL; older
supported systemd versions implement recursive killing internally. Linux documents
that removing a populated cgroup does not terminate it, while `cgroup.kill` kills
the subtree and handles concurrent forks. Emptiness is verified in either case.

## Verification

The ignored integration tests require a real user systemd manager and cgroup v2:

```sh
cargo test -p moonshine-core --lib session::processes -- --include-ignored
cargo test -p moonshine-core --lib process_lifecycle_tests -- --include-ignored
```

They exercise graceful signal handling, post-command completion, detached stubborn
children, cancelled startup and shutdown waiters, launcher exit, lease loss,
session isolation, descriptor inheritance, late-launch rejection, Xlib teardown
ordering, and reservation of the session slot until in-process shutdown finishes.
They create uniquely named test units and do not stop existing Moonshine sessions.

Validation on September 7, 2026: all 202 workspace tests with all features and all
nine systemd integration tests passed, along with Clippy, Rustfmt, documentation,
Cargo Machete, and Nix formatting checks. Two optimized GPU smoke cycles using
Wayland vkcube, Xwayland, and H.264 encoding completed at approximately 60 FPS;
both shut down successfully and left no session processes. The running Moonshine
service was not restarted or updated.

A focused default-feature compositor pacing test failed on both this change and
the untouched parent revision (`84dd22b88726be29364342a7308cd0550a46f7cf`):
`override_pacing_survives_popup_and_focus_transitions_without_capture`. The
all-features suite passed. Short debug-build GPU smoke runs also hit the existing
video FEC initialization cost before their shutdown deadline; the optimized runs
completed that initialization and shutdown normally. Neither result is a claim
of end-to-end Moonlight client validation.

## Sources

- [Kernel cgroup v2 documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html): process inheritance, population events, and `cgroup.kill`.
- [Systemd cgroup delegation](https://systemd.io/CGROUP_DELEGATION/): ownership, delegation, services, scopes, and slices.
- [Systemd service documentation](https://github.com/systemd/systemd/blob/main/man/systemd.service.xml): `ExitType`, hooks, and stop timeouts.
- [Systemd unit implementation](https://github.com/systemd/systemd/blob/v261/src/core/unit.c): `KillUnit` and kernel subtree killing.
- [Systemd scope runner](https://github.com/systemd/systemd/blob/v261/src/run/run.c): join scope before `execvpe`, preserving inherited descriptors.
- [Pinned Smithay Xwayland implementation](https://github.com/hgaiser/smithay/blob/fd6e04e18114bceca6279362661b822b49663fe1/src/xwayland/xserver.rs): PATH resolution, environment, and descriptor inheritance.
