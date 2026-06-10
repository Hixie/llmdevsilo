# Security model

This is a condensed statement of what llmdevsilo defends against, how, and
what it deliberately does not defend against. The authoritative analysis
is in [DESIGN.md](DESIGN.md).

## Premise

The agent is assumed to be *capable and possibly misaligned*. Misalignment
can come from prompt injection in third-party content the model reads,
latent triggers or training-induced misalignment in the model itself, or
plain mistakes in user input. There are **no permission prompts**:
permission fatigue makes prompts useless as a security boundary, so the
boundaries here are static and hold regardless of what the model decides
to do.

## What is protected, and by what

| Attack payload | Mitigation |
| --- | --- |
| Destroying the user's filesystem | The sandbox can write only to the workspace and its scratch space. The host filesystem is reachable read-only, and only the allowlisted parts. |
| Hardware damage, privilege escalation | No root, no device access, minimal kernel surface (strongest in the VM/gVisor backends; macOS sandbox-exec relies on Seatbelt's default-deny). Capabilities like mounting filesystems, changing the clock, or configuring network interfaces are denied. |
| Stealing user credentials/personal data (SSH keys, browser cookies, cloud credentials, other agents' data, conversation logs) | The sandbox cannot read them: they are outside the workspace and outside the read allowlist. The harness additionally refuses to allowlist known-sensitive paths (`silo_core::risk`), including its own state directory (journals, tokens, keys). |
| Stealing the service tokens the agent uses (e.g. GitHub) | The egress proxy injects credentials into outbound requests for specific hosts; the credential values never enter the sandbox. |
| Exfiltrating intranet data | The proxy blocks localhost, RFC 1918, link-local, CGNAT, unique-local, and multicast destinations — checked against every DNS-resolved address (IPv4, IPv6, and IPv4-mapped IPv6), not just host names. Only allowlisted domains resolve/connect at all. |
| Poisoning the user's environment to escape later (git hooks, `.envrc`, IDE config, package-manager scripts) | The workspace is *locked* while the agent works: the user is kept away from the files. Unlocking force-terminates the harness and produces a full diff of every change, with known auto-exec surfaces flagged explicitly. Running the agent's code safely is done through `silo shell`, which puts the *user* in the same sandbox. |

Network egress blocking is load-bearing only for the intranet case. For
everything else it is defense-in-depth: the things worth exfiltrating are
unreadable in the first place. That is why giving the sandbox GitHub
access (or any allowlisted domain) does not create an exfiltration risk,
and why data smuggled through URLs or DNS names is not a concern.

## Accepted risks (out of scope by design)

- **Misaligned code in the output.** The agent writes code; malicious code
  cannot be distinguished from intended code statically. Human review is
  the only mitigation, and `silo shell` exists so review and testing
  happen inside the sandbox.
- **Vandalism of the workspace itself.** The agent can delete anything it
  can write. Locking scopes that damage to the workspace.
- **Temporary host disruption** (CPU, memory, disk pressure).
- **Exfiltration of anything intentionally given to the agent**: the
  workspace contents (assumed open source), allowlisted host files
  (assumed generic binaries/tools), injected development credentials
  (assumed scoped and temporary), and conversation content sent to the
  cloud LLM providers (assumed trustworthy).
- **Kernel bugs.** Kernels are assumed to implement their documented
  semantics; VM-class backends reduce exposure.

## Known weak points

- **User-mediated escape via reviewed output** remains the weakest link:
  a user who copies the agent's instructions or runs its code outside
  `silo shell` leaves the security model.
- **macOS sandbox-exec caveats**: the locked workspace mount is
  technically reachable by the host user while a harness is attached (the
  platform offers no exclusive in-sandbox mount), host loopback services
  are reachable from inside the sandbox, and Seatbelt is deprecated by
  Apple. The harness surfaces these as warnings; the Linux VM backend is
  the stronger alternative.
- **Credential injection coverage**: services whose protocols cannot be
  proxied cannot use injection, and TLS-MITM breaks certificate-pinning
  clients (notably some package managers).
- **Secrets hygiene depends on configuration**: the user chooses what to
  allowlist and which env-var credentials to expose to the proxy. Scope
  tokens minimally.

## Logging

Journals record all module interactions (prompts, LLM requests/responses,
tool executions, network metadata) at a level sufficient to regenerate a
deterministic mock-component test of the session. They are designed to
carry no secrets: credentials exist in configuration only as environment
variable *names*, the proxy journals metadata (host, method, path without
query, status, byte counts) rather than bodies, and `SecretString` values
serialize as `[redacted]`. Journals live in the state directory, which the
sandbox cannot read.
