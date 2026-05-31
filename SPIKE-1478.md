# #1478 Analysis Spike — `--features discord` won't compile (twilight version split)

**Author:** fixup-dev-2 · **Status:** spike (read-only) · **Base:** main @ 0d3819d

## TL;DR

`Cargo.toml` pins **`twilight-model = "0.17"`** while **`twilight-gateway`/`twilight-http` = "0.16"`**. The twilight crates version in lockstep, so gateway/http 0.16 pull `twilight-model` **0.16** transitively → two semver-incompatible copies of `twilight-model` coexist → every place our code hands a 0.17 `Id<…>` to a 0.16 gateway/http API fails to type-check. **Fix: align `twilight-model` down to `"0.16"` — one line, pure Cargo.toml, no code changes. Verified: `cargo build --features discord` Finishes clean after the change.** Plus: add a `--features discord` build job to CI (none exists today — that's why this regressed unseen).

## 1. The exact errors (origin/main @ 0d3819d)

`cargo build --features discord` → **8× `error[E0308]` + 1 compile-failure line = 9**. All are the same class:

```
src/channel/discord.rs:549:  req = req.parent_id(twilight_model::id::Id::new(pid));
   |  expected `Id<ChannelMarker>`, found `Id<_>`
... expected `Id<ChannelMarker>`, found `Id<_>`   (×N)
... expected `Id<GuildMarker>`,   found `Id<_>`
... "arguments to this method are incorrect"        (×2)
```

`twilight_model::id::Id::new()` in our code resolves to **0.17**; the `req` builder is `twilight-http` **0.16**, expecting a **0.16** `Id<ChannelMarker>`. Same nominal type, different crate version → distinct types → mismatch.

## 2. Dependency-tree source (`cargo tree --features discord -i twilight-model`)

`twilight-model` is **ambiguous** — two versions resolved:

```
twilight-model v0.16.0           twilight-model v0.17.1
├── twilight-gateway v0.16.0     └── agend-terminal v0.7.0   (DIRECT dep)
├── twilight-http v0.16.0
└── twilight-validate v0.16.0
    └── twilight-http v0.16.0
```

- **0.16** = transitive, via `twilight-gateway`/`twilight-http`/`twilight-validate` 0.16.
- **0.17** = our **direct** `twilight-model = "0.17"` dep.

Likely origin: deps were all 0.16; later `twilight-model` alone got bumped to 0.17 (manual or auto-update) without realigning gateway/http. CI never builds `--features discord` (see §5), so it landed unnoticed.

## 3. Solution options + trade-off

| Option | Change | KISS | Risk |
|---|---|---|---|
| **A. Align DOWN — `twilight-model = "0.16"`** (RECOMMENDED) | 1 line in Cargo.toml | ★★★ | **None — verified compiles.** Stays on twilight 0.16 (coherent with gateway/http). |
| B. Align UP — bump gateway/http to `"0.17"` | 2-3 lines + likely code edits | ★ | Requires twilight-gateway/http **0.17 to exist** (unconfirmed — registry mirror blocked `cargo search`); 0.16→0.17 has gateway-event / http-builder API changes → probable code churn. |
| C. `[patch]` / pin a single version | extra section | ★ | Patching to force one version across a semver gap usually fails to compile (the APIs genuinely differ); more machinery than A. |

**Recommend A.** The discord feature uses only stable `twilight_model` API (`channel`, `gateway`, `gateway::payload::incoming`, `id`) present identically in 0.16 — so dropping to 0.16 needs **zero code changes** (verified). Align-up (0.17 wholesale) is a legitimate *separate* follow-up if 0.17 features are ever wanted, but it's not the KISS unblock.

## 4. Pure Cargo.toml or code changes?

**Pure Cargo.toml.** Verified empirically: changed `twilight-model = { version = "0.17" … }` → `"0.16"`, ran `cargo build --features discord` → **`Finished` (0 errors)**, reverted. No `.rs` edits required.

## 5. RED / regression guard

**CI has NO `--features discord` build job** — `ci.yml` only compiles/tests `--features tray`. So discord code is never type-checked in CI; this conflict (and the #1476/#1477 discord `block_on` fixes that couldn't be locally verified) rode along invisibly.

RED→GREEN: today `cargo build --features discord` fails; after Option A it Finishes. To prevent regression, **add a CI step** (cheap — `cargo check`, not full build/test):

```yaml
# in ci.yml, alongside the tray check
- name: Discord feature compiles (#1478)
  run: cargo check --features discord --quiet
```

`cargo check` (not `build`) keeps it fast — it only needs to type-check, which is exactly what catches the version split. Gate it on one OS (ubuntu) to keep CI minutes low; the conflict is platform-independent.

## KISS assessment

Option A is maximally KISS: one character-range edit (`0.17`→`0.16`), zero code, verified green. The only added surface is the regression-guard CI step (≈3 lines), which is the actual long-term value — it turns "discord silently rots" into "discord must compile." Recommend shipping A + the CI `cargo check` step as one small PR.
