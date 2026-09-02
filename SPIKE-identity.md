# MCP identity under one OS user

## Decision

Accept and document the current trust model: an AgEnD agent seat is trusted code
running as the operator's OS user. Keep live-name resolution as protection against
mistakes and stale callers, but do not describe it as authentication between
hostile seats. A per-instance secret inside the present shared-UID design would
add lifecycle machinery without creating a meaningful security boundary.

If hostile agent code becomes an explicit threat, the next design should be an
optional OS-isolated execution mode, not another shared-UID token.

## Threat model and present boundary

There are two materially different adversaries:

- A confused or buggy agent may send the wrong name, use a dead identity, or call
  an operation outside its role. Live-registry lookup and role checks are useful
  here.
- A deliberately hostile seat can execute arbitrary commands as the same uid as
  the daemon and every peer. It is already effectively the operator; stopping
  only request-name impersonation does not contain it.

The claimed identity path is explicit. The bridge reads `AGEND_INSTANCE_NAME`
from its environment (`src/bin/agend-mcp-bridge.rs:71-79`), and the wire envelope
places that string in `params.instance` (`src/mcp_wire.rs:253-266`). The daemon's
lookup checks only that the claimed UUID/name identifies exactly one live handle;
its own comment says another live peer can be named
(`src/api/handlers/mcp_proxy.rs:72-93`). The handshake PID is logged and watched
for liveness, not used as identity (`src/api/mod.rs:659-690`).

A hostile same-uid seat needs no impersonation to:

- Rewrite `fleet.yaml`, runtime records, inbox/task data, or MCP configuration.
  Fleet configuration is `<home>/fleet.yaml` (`src/fleet/mod.rs:45-50`), while
  workspace and binding state are under the same home (`src/paths.rs:5-17`).
- Rewrite another managed worktree: their common root is
  `<home>/worktrees/<agent>/<branch>` (`src/worktree_pool.rs:69-76`).
- Read both daemon credentials. `api.operator` grants full operator capability
  (`src/auth_cookie.rs:22-47`), and the source already records that mode `0600`
  does not isolate another process with the same uid
  (`src/auth_cookie.rs:55-69`). The operator principal can invoke every API
  method (`src/api/operator_gate.rs:148-165`).
- Signal/terminate peer processes, replace shared executables/configuration, or
  edit state/audit files before the daemon consumes them. The daemon itself keeps
  peer PIDs in the live registry (`src/instance_monitor.rs:128-143`).

### Host measurement (2026-09-02)

On this Darwin 24.5.0 arm64 host, uid 501, a controlled sibling-process probe
produced:

```text
ps eww / ps Eww / ps e: test secret not visible
KERN_PROCARGS2: self secret visible; sibling secret not visible
/proc/<pid>/environ: absent
kill -0 <same-uid sibling>: permitted
fleet.yaml and worktrees: owner-writable
api.cookie and api.operator: 0600, same owner
```

Thus macOS currently prevents the tested cross-process environment read, but it
does not protect same-owner files or processes. On Linux,
[`/proc/<pid>/environ`](https://man7.org/linux/man-pages/man5/proc_pid_environ.5.html)
is guarded by `PTRACE_MODE_READ_FSCREDS`; the result depends on matching
credentials, dumpability, capabilities, and LSM/Yama policy rather than being a
portable secret boundary.

## Options and cost

### A. Per-instance capability secret — medium cost, weak hostile-seat value

Mint at backend spawn, deliver to the bridge, verify on each connection/call,
and rotate on restart. The spawn path already re-injects identity and home after
environment clearing (`src/agent/mod.rs:891-927`), while MCP configuration is
currently a project file containing the bridge command and environment
(`src/mcp_config.rs:85-98`, `src/mcp_config.rs:728-739`). Implementation must add
issuance, storage, constant-time verification, reconnect invalidation, rotation,
redaction, and backend-specific tests.

An environment-only secret is not exposed by the measured macOS APIs, so this
would reduce accidental cross-seat calls. Persisting it in MCP config immediately
loses that benefit. Even if kept only in memory, a hostile seat can read the
existing operator token, rewrite the target config/worktree or bridge, and signal
processes. Linux exposure is host-policy-dependent. This is defense against
confusion, not a hostile same-uid boundary.

### B. Server-bound connection identity — high cost, incomplete containment

Identity would come from a daemon-created connection associated with its spawn
record, never from a request field. Today the backend launches the configured
stdio bridge, which lazily opens a shared-cookie loopback TCP connection
(`src/bin/agend-mcp-bridge.rs:71-79`, `src/mcp_wire.rs:283-317`). Making the daemon
own that relationship requires redesigning spawn/supervision, bridge launch,
reconnect and restart handoff—likely a daemon-spawned bridge plus a securely
inherited channel or broker stub.

This removes name spoofing and improves attribution. It still does not stop a
same-uid attacker from editing shared state/code, reading `api.operator`, or
killing processes, so it is not sufficient for the stated hostile-seat model.

### C. OS boundary — very high cost, actual containment

Run seats under distinct OS identities, or under enforced per-seat sandboxes.
Separate uids give file DAC and signal isolation; Linux additionally needs mount
and user namespace design. A supported
[macOS App Sandbox](https://developer.apple.com/documentation/security/app-sandbox)
requires signed entitlements and container/file grants; Apple's
[sandbox inheritance guidance](https://developer.apple.com/library/archive/documentation/Miscellaneous/Reference/EntitlementKeyReference/Chapters/EnablingAppSandbox.html)
says an embedded CLI helper inherits the app sandbox and recommends XPC for
privilege separation.

This conflicts with the current shared `$AGEND_HOME`: each seat would need private
credentials/config/cache and worktree ownership, while task/inbox/git operations
move behind a narrowly authorized broker. Git credentials/signing, repository
access, PTY control, tool execution, and daemon upgrades all need explicit grants.
It is the only option here that addresses the hostile-seat threat, but it is a
separate execution architecture rather than a small MCP fix.

## Minimal boundary test

Keep one behavioral test named
`same_uid_live_orchestrator_name_claim_is_trusted_by_design`: register two live
handles, authenticate through the shared agent MCP transport, claim the current
orchestrator's live name, and assert that the handover is accepted. Its comment
must cite `SAME_UID_OPERATOR_ISOLATION == Unresolved` and state that the test pins
an accepted limitation, not a security guarantee. If OS isolation is later
implemented, deliberately invert this test and require cryptographic/OS-bound
caller evidence before changing the constant to `Resolved`.
