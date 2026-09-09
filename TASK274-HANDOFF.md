# Task274 handoff

Status: implementation and local verification complete; awaiting independent
orchestrator review. Do not push, merge, or close #3539 from this handoff.

- Branch: `fix/3559-stop-transport-regression-274`
- Base: `26a533bebf5d5274ee641a07e232aaaf02d62f15`
- RED test commit: `1374b66af62589dec7650c08598196e7821c24dc`
- Immutable corrected-fixture RED commit: `510ae93ecc5ef59eb24409413fd2c24233d794e9`
- Contained-fixture commit: `57897984e42bb656e93770a43096a4d33c85ffa6`
- GREEN fix commit: `bb661dc7d7693b815981d3582acaa674fae477ab`
- Current branch head: `e3edc914fabd699201535f580d77552ff35f7d25`
- Evidence/log artifact: `TASK274-EVIDENCE.md`
- Production change: API transport failure after active run discovery now
  returns an explicit error; absent discovery remains idempotent; accepted
  `--no-wait` behavior is unchanged.
- Validation: transport regression 1 pass, no-wait control 1 pass,
  `cli_stop::tests` 6 pass, no-daemon CLI exit 0, package fmt/clippy/diff
  checks pass.
- Not done: old leak-prone `PlantedResidue` execution and full #3539
  test-runner cleanup remain separate obligations.
- Fixture safety: `UniqueFixtureHome` plus an immediately-held foreground
  `Child` guard; empty fleet, no descendants, exact-child bounded teardown.
  Normal teardown asserts `reap() -> Result`, confirms the PID is gone, then
  asserts exact-home cleanup; panic cleanup is best-effort and diagnostic.
