# Sandbox backends

This document describes the four platform sandbox backends in
`crates/silo-sandbox` (the fifth backend, `mock`, executes nothing and is
described in `crates/silo-sandbox/src/mock.rs`). For the requirements they
implement, see [DESIGN.md](DESIGN.md); for the surrounding runtime
composition, see [ARCHITECTURE.md](ARCHITECTURE.md).

Every backend provides the same contract (`silo_core::traits::Sandbox`):

- a read/write **workspace** (the attached locked workspace),
- a read/write per-session **scratch space**, created on start and removed
  on shutdown, holding the sandboxed `HOME`, `TMPDIR`, and the session CA
  certificate `proxy-ca.pem`,
- read-only access to the user-configured **allowlist** plus an OS
  baseline, and nothing else,
- network egress **only through the harness proxy**,
- a **helper process** (`silo-helper`, untrusted, running inside the
  sandbox) that executes the Read/Write/Edit/Bash/WebFetch/WebSearch
  tools, and
- an **access report** describing all of the above for user inspection.

## macos-sandbox-exec

Status: implemented (`src/macos/profile.rs`, `src/macos/sandbox_exec.rs`).
This is the native-process backend for developers building macOS software
inside the sandbox.

**Trust model.** Enforcement is the macOS Seatbelt kernel extension,
driven by a generated SBPL profile and applied by `/usr/bin/sandbox-exec`.
The helper and every process it spawns inherit the profile; nothing inside
the sandbox runs with privileges. `sandbox-exec` is deprecated by Apple
but still functional; the profile starts from `(deny default)`, so an
operation Apple removes from the allow vocabulary fails closed.

**Workspace, scratch, and allowlist invariants.** The profile allows
`file-write*` only on the workspace subpath, the scratch subpath, and
`/dev/null` (plus `file-write-data` on `/dev/dtracehelper`, which
libSystem opens at process start). `file-read*` covers the workspace, the
scratch, each allowlist entry, and a fixed OS baseline (`/usr/lib`,
`/usr/share`, `/usr/bin`, `/bin`, `/usr/sbin`, `/sbin`, `/System`,
`/Library/Preferences/Logging`, `/private/etc`, `/private/var/db/dyld`,
`/private/var/db/timezone`, and a handful of device literals). All paths
are canonicalized before profile generation because Seatbelt matches
resolved paths. Paths containing a double quote or a newline are rejected
at setup time, so profile injection through path names is not possible.
`file-read-metadata` is allowed globally so path traversal works; this
leaks the existence and attributes of files outside the allowlist, and the
access report says so.

**Workspace attachment.** The attached workspace directory is used as-is
(the workspace manager hands the backend a host directory). While
attached it is technically reachable by the host user; the access report
carries a note telling the user not to touch it from outside.

**Helper transport.** A Unix socket, `<scratch>/helper.sock`. The backend
binds it, spawns `sandbox-exec -f <profile> silo-helper
unix:<scratch>/helper.sock`, and accepts the connection (15 second
timeout). The Unix-socket connect is covered by an explicit
`network-outbound (subpath <scratch>)` rule. The profile file itself
lives in a private 0700 temporary directory outside the scratch, readable
only by `sandbox-exec`, which runs outside the sandbox.

**Network funneling.** The profile denies all network operations except:
outbound to `localhost:*`, inbound/bind on `localhost:*`, and Unix-socket
connections inside the scratch. The helper is spawned with a cleared
environment plus `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` pointing at the
harness proxy on loopback and the CA-bundle variables pointing at
`proxy-ca.pem`, so well-behaved tools reach the proxy and everything else
is refused by Seatbelt. Two consequences, both noted in the access
report: programs that ignore proxy environment variables simply cannot
reach the network (unavoidable with this design), and services listening
on the host loopback interface are reachable from inside the sandbox.

**User shell.** `user_shell` runs the user's `$SHELL -i` (or an explicit
command vector) under the same profile, same cleared environment (plus
`TERM`), with the workspace as the working directory — the user is
sandboxed exactly like the model.

## macos-linux-vm

Status: design scaffold (`src/macos/linux_vm.rs`); `create` returns
`SandboxError::Unavailable`.

**Trust model.** A minimal Linux guest booted through
Virtualization.framework. Enforcement is the hypervisor boundary: the
guest kernel is untrusted, and only the devices the harness attaches are
reachable. This is the stronger-isolation option on macOS, for work that
does not need to build or run macOS-native binaries.

**Workspace, scratch, allowlist.** The workspace is attached read/write
over virtio-fs; each allowlist entry is its own read-only virtio-fs
share; the scratch space is a guest-local filesystem that is destroyed
with the VM, so it is never accessible from the host at all.

**Helper transport.** vsock. The harness listens on a vsock port and the
guest init launches the helper with a `vsock:` connect string (the helper
protocol is transport-agnostic JSON lines).

**Network funneling.** The guest gets one virtio-net interface whose
host side is a harness-owned tap. The harness forwards guest connections
to port 3128 to the egress proxy and drops everything else (including
DNS), so the proxy's domain allowlist sees hostnames via CONNECT and
nothing can bypass it. Host loopback services are not reachable from the
guest, unlike under sandbox-exec.

## linux-gvisor

Status: implemented, not yet exercised on a Linux host
(`src/linux/spec.rs`, `src/linux/forward.rs`, `src/linux/gvisor.rs`).
Specification generation is pure and unit-tested; the runtime drives
`runsc`.

**Trust model.** gVisor's user-space kernel (`runsc`) intercepts all
syscalls; sandboxed code never talks to the host kernel directly. The
container runs with no capabilities, `noNewPrivileges`, a read-only
empty rootfs, and standard masked/read-only `/proc` and `/sys` paths.

**Workspace, scratch, allowlist invariants.** Everything visible inside
the sandbox is an explicit bind mount in the generated OCI `config.json`:
OS directories (`/usr`, `/bin`, `/lib`, `/lib64` when present, `/etc/ssl`,
`/etc/alternatives`) and the allowlist entries read-only at their own
paths; the workspace read/write at `/workspace`; the scratch read/write at
`/scratch` (and additionally at its host path, so the host-path `HOME` and
`TMPDIR` values the tool layer sets per execution stay valid inside).
Paths outside the mounts do not exist in the sandbox — not even their
metadata, which is stronger than the sandbox-exec backend.

**Helper transport.** A Unix socket, `<scratch>/helper.sock`, reachable
inside as `/scratch/helper.sock` through the scratch bind mount (`runsc`
is run with `--host-uds=all`). The helper binary is copied into
`<scratch>/bin/` before start (`SILO_HELPER_BIN_LINUX` selects a binary
when the harness itself is not running on the same kind of machine).

**Network funneling.** `runsc --network=none`: the sandbox has a loopback
interface but no external connectivity and no DNS. The helper starts a
relay on `127.0.0.1:3128` inside (configured by
`SILO_PROXY_RELAY=/scratch/proxy.sock:3128`) that pipes byte-for-byte to
`/scratch/proxy.sock`; the harness side forwards that socket to the
egress proxy's TCP address. That relay is the only egress path. Because
clients speak HTTP proxy protocol to it, hostnames travel inside CONNECT
requests and all name resolution happens in the proxy — in-sandbox DNS
does not exist, which satisfies the DNS-proxying requirement by
construction. Host loopback services are not reachable from the sandbox.

**User shell.** `user_shell` uses `runsc exec` into the running
container with the workspace as the working directory.

## linux-microvm

Status: design scaffold (`src/linux/microvm.rs`); `create` returns
`SandboxError::Unavailable`.

**Trust model.** A Firecracker-style microVM: a real guest kernel under
KVM. Like the macOS Linux VM, the boundary is the hypervisor, which
trades the convenience of bind mounts for stronger isolation than
gVisor's user-space kernel.

**Workspace, scratch, allowlist.** The workspace attaches over virtio-fs
(or as a virtio-blk device when the workspace container file is used
directly, which also gives loop-device-style locking); allowlist entries
are read-only virtio-fs shares; the scratch space is a guest-local
filesystem destroyed with the VM.

**Helper transport.** vsock, as in the macOS Linux VM design.

**Network funneling.** The guest's default route points at a host-side
tap owned by the harness; the harness forwards port 3128 to the egress
proxy and drops all other traffic, including DNS. Identical posture to
the macOS Linux VM: hostname resolution happens only in the proxy, and
host loopback services are unreachable from the guest.

## Comparison

| | sandbox-exec | linux-vm (macOS) | gvisor | microvm |
| --- | --- | --- | --- | --- |
| Enforcement | Seatbelt kernel policy | hypervisor | user-space kernel | hypervisor (KVM) |
| Native macOS binaries | yes | no | no | no |
| Helper transport | Unix socket | vsock | Unix socket | vsock |
| Workspace attachment | host directory | virtio-fs | bind mount | virtio-fs / virtio-blk |
| Metadata of unlisted host paths | visible | hidden | hidden | hidden |
| Host loopback reachable from sandbox | yes (noted in report) | no | no | no |
| DNS inside the sandbox | host resolver, egress blocked | none (proxy resolves) | none (proxy resolves) | none (proxy resolves) |
| Status | implemented | scaffold | implemented (untested on Linux hosts) | scaffold |
