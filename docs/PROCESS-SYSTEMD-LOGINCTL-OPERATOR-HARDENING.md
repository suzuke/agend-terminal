# systemd `loginctl enable-linger` — operator hardening for Linux service install

**Sprint 63 W2 PR-2 — operator how-to.** Closes Sprint 58 P2 #2
deferred carryover per Sprint 57 Phase 3 #557 Pass 2 r1 note.
Operator-facing companion to `agend-terminal service install` on
Linux (systemd user units).

---

## Why this matters

`agend-terminal service install` on Linux registers the daemon as a
systemd **user** unit at
`~/.config/systemd/user/agend-terminal-daemon.service`. By default,
systemd user instances are bound to login sessions: the user manager
(`systemd --user`) starts when the operator logs in and stops when
the operator's last session ends.

Without `loginctl enable-linger`, the daemon will:

- Auto-start on login ✓
- **Stop when the operator logs out** ✗ — even if the daemon was
  installed for "auto-start at boot" semantics

This breaks the Q3 NOT-self-supervisor + OS-owns-lifecycle invariant
for headless servers, multi-user boxes, or any deployment where the
operator isn't continuously logged in.

---

## What enable-linger does

`loginctl enable-linger <user>` instructs systemd-logind to start the
target user's user-manager **at boot** (regardless of login state)
and keep it running independent of login sessions. With linger
enabled, the agend-terminal daemon's user-unit activation persists
across logout/login cycles and survives a reboot for headless boxes.

Per `loginctl(1)` and `systemd-logind.service(8)`: linger is a
per-user persistent flag stored in `/var/lib/systemd/linger/<user>`.

---

## Operator commands

### Enable (one-time, after `agend-terminal service install`)

```bash
loginctl enable-linger
# OR explicitly:
loginctl enable-linger "$USER"
```

No sudo / root required for the operator's own user. Idempotent.

### Verify

```bash
loginctl show-user "$USER" --property=Linger
# Expected output: Linger=yes
```

### Disable (operator opt-out)

```bash
loginctl disable-linger
```

This stops the daemon at the next logout (per default systemd user-
unit semantics). To uninstall the service entirely, use
`agend-terminal service uninstall` instead.

---

## When to enable

Enable linger when ANY of the following applies:

- Headless server / SSH-only deployment where the operator isn't
  continuously logged in
- Multi-user host where the agend-terminal user has no interactive
  session most of the time
- The daemon needs to survive operator logout (typical production
  posture for any always-on agent)

Skip linger when the daemon is intentionally session-scoped (e.g.
desktop development workflow where logging out should stop the
daemon as a clean-state reset).

---

## Cross-platform parity

- **macOS (launchd)**: no equivalent step. `launchctl load -w`
  registers the LaunchAgent for the user; macOS persists the
  registration across logout/login automatically.
- **Windows (Task Scheduler)**: no equivalent step. The `at-logon`
  trigger registered by `agend-terminal service install` already
  fires per session.
- **Linux (systemd user)**: `enable-linger` is **the** required
  one-time hardening step. Document this prominently in any operator
  runbook that includes `agend-terminal service install` for Linux.

---

**Summary.** `agend-terminal service install` on Linux ships a
systemd user unit; without `loginctl enable-linger`, the daemon
stops at logout. One-time `loginctl enable-linger` (no root needed)
is the operator-hardening step that completes the cross-platform
"OS-owns-lifecycle" invariant. macOS / Windows have no equivalent
step.
