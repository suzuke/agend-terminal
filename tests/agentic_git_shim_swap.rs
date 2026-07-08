//! #2524 P2 (agentic-git migration, design §5-Phase-2): parity coverage for the
//! flag-gated git-shim backend swap (`use_agentic_git_shim`).
//!
//! Three tiers:
//!   1. SOURCE parity (hermetic, always runs): the vendored `agentic-git` shim
//!      source must carry every git guard agend-terminal depends on AND read
//!      every daemon-injected `AGEND_*` env var (legacy fallback), so pointing
//!      `git` at it (flag on) preserves the guard matrix. Mirrors the existing
//!      `shim_*_in_source` checks for agend-git (agend_git_shim_phase2.rs).
//!   2. RUNTIME guard (gated on a prebuilt binary): run the real binary as `git`
//!      and prove the unbound-mutation deny actually fires.
//!   3. CROSS-BUILD HMAC byte-compat (gated): a binding SIGNED by the daemon's
//!      core (agend-terminal build, via the dev-dep) is VERIFIED + ALLOWED by the
//!      SUBMODULE-built binary — the load-bearing safety assumption of the flag
//!      flip — plus a tamper negative-control so the ALLOW can't be a rubber-stamp.
//!
//! The gated tiers prebuild the binary via `scripts/build_agentic_git_shim.sh`
//! (CI runs it before nextest; locally skip-loud if absent). We deliberately
//! NEVER nested-`cargo build` inside a test — that reintroduced the windows
//! target-lock deadlock removed in agend_git_shim_phase2.rs:42-48. The vendored
//! source is already a hard build prerequisite (the daemon links
//! `agentic-git-core` from this same submodule, P1b), so `include_str!` on it
//! adds no new fragility.

const VENDORED_SHIM: &str = include_str!("../vendor/agentic-git/crates/agentic-git/src/main.rs");

/// Every git guard agend-terminal relies on must be present in the vendored
/// successor shim (design §5 DUAL parity core). A missing one = a silent guard
/// regression the moment the flag flips on.
#[test]
fn vendored_shim_carries_every_depended_on_git_guard() {
    for (needle, why) in [
        ("fn deny_unbound_else_chdir", "unbound → deny all mutations"),
        (
            "fn push_protected_violation",
            "protected-ref (main/master) deny",
        ),
        (
            "fn push_trust_root_denylist_violation",
            "trust-root push denylist",
        ),
        (
            "fn push_force_without_lease_violation",
            "force-lease gate (P0)",
        ),
        (
            "fn is_pure_delete_push",
            "#2677 ALL-not-ANY mixed-refspec force fix",
        ),
        ("fn cwd_is_canonical_rooted", "canonical-repo protection"),
        ("recursion_guard_or_exit", "self-resolution recursion guard"),
        ("cross-branch", "checkout/switch cross-branch fence"),
        (
            "integrity_core::verify",
            "HMAC binding verify (fail-closed)",
        ),
        ("fn read_binding", "fail-closed binding read"),
    ] {
        assert!(
            VENDORED_SHIM.contains(needle),
            "vendored agentic-git shim missing `{needle}` ({why}) — flipping \
             use_agentic_git_shim on would regress this guard. Re-pin the submodule."
        );
    }
}

/// The daemon injects only `AGEND_*` names; the vendored shim must read every
/// one (via its `AGENTIC_GIT_*`-primary + `AGEND_*`-legacy fallback), else the
/// swap silently loses bypass / real-git / binding routing.
#[test]
fn vendored_shim_reads_every_daemon_injected_agend_env() {
    assert!(
        VENDORED_SHIM.contains("fn legacy_env_name"),
        "vendored shim must map AGENTIC_GIT_* → AGEND_* legacy names"
    );
    for var in [
        "AGEND_HOME",
        "AGEND_INSTANCE_NAME",
        "AGEND_REAL_GIT",
        "AGEND_GIT_BYPASS",
        "AGEND_GIT_BYPASS_AGENT",
        "AGEND_GIT_BYPASS_UNTIL",
        "AGEND_GIT_SHIM_DEPTH",
        "AGEND_GIT_ALLOW_CANONICAL_MUTATE",
    ] {
        assert!(
            VENDORED_SHIM.contains(&format!("\"{var}\"")),
            "vendored shim does not read daemon-injected {var} under any name — \
             swapping the git shim would drop it"
        );
    }
}

// ── Runtime tiers (gated on a prebuilt binary; unix-only) ────────────────────

#[cfg(unix)]
mod runtime {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};

    /// The prebuilt binary, or None (caller skips-loud). CI exports
    /// AGENTIC_GIT_SHIM_BIN; otherwise fall back to the build script's install
    /// path or the submodule target.
    pub fn locate() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("AGENTIC_GIT_SHIM_BIN") {
            let p = PathBuf::from(p);
            if p.exists() {
                return Some(p);
            }
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        [
            root.join("target/debug/agentic-git"),
            root.join("vendor/agentic-git/target/debug/agentic-git"),
        ]
        .into_iter()
        .find(|c| c.exists())
    }

    /// First existing STANDARD absolute real-git path (never a PATH shim). The
    /// test may run inside an agend session whose PATH `git` is itself a shim, so
    /// we must not resolve real git via PATH — that would feed the shim its own
    /// path (recursion) and let a shim intercept fixture setup.
    pub fn real_git() -> PathBuf {
        [
            "/usr/bin/git",
            "/opt/homebrew/bin/git",
            "/usr/local/bin/git",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("git"))
    }

    /// Build a throwaway git repo with one commit, using REAL git directly
    /// (absolute path → bypasses any PATH shim; real git ignores AGEND_* env).
    pub fn init_repo(dir: &Path) {
        let git = real_git();
        let run = |args: &[&str]| {
            assert!(
                Command::new(&git)
                    .args(args)
                    .current_dir(dir)
                    .env("HOME", dir) // isolate from ~/.gitconfig
                    .status()
                    .expect("fixture git")
                    .success(),
                "fixture git {args:?} must succeed"
            );
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "seed",
        ]);
    }

    /// Run the prebuilt binary as argv[0] `git`, as `agent` bound to `home`, in
    /// `cwd`, with a HERMETIC env: `env_clear` so no inherited `AGEND_GIT_BYPASS`
    /// / shim-depth leaks in and masks the real guard/verify path.
    pub fn run_as_git(bin: &Path, home: &Path, agent: &str, args: &[&str], cwd: &Path) -> Output {
        use std::os::unix::fs::symlink;
        let git = home.join("git");
        let _ = std::fs::remove_file(&git);
        symlink(bin, &git).expect("symlink git → agentic-git");
        let real = real_git();
        let real_dir = real.parent().unwrap_or_else(|| Path::new("/usr/bin"));
        Command::new(&git)
            .args(args)
            .current_dir(cwd)
            .env_clear()
            .env("PATH", real_dir)
            .env("HOME", home)
            .env("AGEND_HOME", home)
            .env("AGEND_INSTANCE_NAME", agent)
            .env("AGEND_REAL_GIT", &real)
            .output()
            .expect("run agentic-git as git")
    }

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("agentic-p2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir scratch");
        base
    }

    /// Build a temp AGEND_HOME with a REAL bound worktree and a binding SIGNED by
    /// agend-terminal's `agentic_git_core` (the daemon's signer, via the dev-dep).
    /// Returns `(home, agent)`.
    fn daemon_signed_bound_home(tag: &str) -> (PathBuf, String) {
        let base = scratch(tag);
        let agent = "bound-agent".to_owned();
        let wt = base.join("wt");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        init_repo(&wt);

        // The daemon's field shape; the shim only requires a non-empty task_id +
        // an existing worktree. Sign the EXACT bytes written — sign_binding hashes
        // raw bytes verbatim (no canonicalization).
        let body = format!(
            "{{\"version\":1,\"agent\":\"{agent}\",\"task_id\":\"T-xbuild-1\",\
             \"branch\":\"feat/p2-shim-swap\",\"worktree\":\"{}\"}}",
            wt.display()
        );
        let sig = agentic_git_core::integrity_core::sign_binding(&base, body.as_bytes())
            .expect("agend-terminal-build core must sign");

        let bdir = base.join("runtime").join(&agent);
        std::fs::create_dir_all(&bdir).expect("mkdir runtime/agent");
        std::fs::write(bdir.join("binding.json"), &body).expect("write binding.json");
        std::fs::write(bdir.join("binding.json.sig"), &sig).expect("write sig");
        (base, agent)
    }

    /// Runtime guard: unbound mutation is DENIED (exit != 0, "denied … unbound").
    /// Proves flipping the flag routes `git` to a binary that actually guards.
    #[test]
    fn unbound_mutation_is_denied() {
        let Some(bin) = locate() else {
            eprintln!("SKIP unbound_mutation_is_denied: no prebuilt agentic-git binary");
            return;
        };
        let base = scratch("unbound");
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir repo");
        init_repo(&repo);
        // AGEND_HOME=base has no runtime/<agent> binding → unbound.
        let out = run_as_git(
            &bin,
            &base,
            "ghost-unbound",
            &["commit", "--allow-empty", "-m", "x"],
            &repo,
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "unbound `git commit` must be DENIED (nonzero exit). stderr={stderr}"
        );
        assert!(
            stderr.contains("denied") && stderr.to_lowercase().contains("unbound"),
            "deny message must name the unbound reason; stderr={stderr}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Cross-build proof: a daemon-core-signed BOUND binding is verified and
    /// ALLOWED by the submodule-built binary (`git add .` → exit 0), proving the
    /// two independent `agentic-git-core` compiles are HMAC byte-compatible.
    #[test]
    fn daemon_signed_binding_allowed_across_core_builds() {
        let Some(bin) = locate() else {
            eprintln!("SKIP daemon_signed_binding_allowed_across_core_builds: no prebuilt binary");
            return;
        };
        let (home, agent) = daemon_signed_bound_home("allow");
        // `git add .` from a NON-git cwd (home) → bound → ChdirPass into the
        // worktree → real `git add` (exit 0 on a clean tree). A non-git cwd
        // avoids the foreign-repo passthrough that would mask the bound path.
        let out = run_as_git(&bin, &home, &agent, &["add", "."], &home);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "daemon-core-signed bound binding must be VERIFIED + ALLOWED by the submodule-built \
             binary (cross-build HMAC byte-compat); exit={:?} stderr={stderr}",
            out.status.code()
        );
        assert!(
            !stderr.contains("denied"),
            "a validly-bound agent must not be denied; stderr={stderr}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Negative control: flipping one hex char of the daemon signature makes the
    /// same bound `git add` DENY — proving the ALLOW above is the HMAC actually
    /// verifying (fail-closed), not the binary rubber-stamping any binding.
    #[test]
    fn tampered_daemon_signature_is_denied() {
        let Some(bin) = locate() else {
            eprintln!("SKIP tampered_daemon_signature_is_denied: no prebuilt binary");
            return;
        };
        let (home, agent) = daemon_signed_bound_home("tamper");
        let sigpath = home.join("runtime").join(&agent).join("binding.json.sig");
        let sig = std::fs::read_to_string(&sigpath).expect("read sig");
        std::fs::write(&sigpath, flip_first_hex(&sig)).expect("write tampered sig");

        let out = run_as_git(&bin, &home, &agent, &["add", "."], &home);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success(),
            "a tampered signature must be DENIED (fail-closed); exit={:?} stderr={stderr}",
            out.status.code()
        );
        assert!(
            stderr.to_lowercase().contains("unbound") || stderr.contains("denied"),
            "tampered sig → fail-closed unbound/deny; stderr={stderr}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Flip the first hex digit of a bare-hex tag to a different value.
    fn flip_first_hex(tag: &str) -> String {
        let mut chars: Vec<char> = tag.trim().chars().collect();
        if let Some(first) = chars.first_mut() {
            *first = if *first == '0' { '1' } else { '0' };
        }
        chars.into_iter().collect()
    }
}
