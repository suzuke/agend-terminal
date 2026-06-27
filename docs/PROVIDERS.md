# Model providers (harness × provider)

Agend-Terminal keeps backend detection on two axes:

1. **CLI harness** — the executable that owns the PTY/tool loop, detected from `PATH`.
2. **Model provider** — the hosted/local token endpoint configured through that harness.

A provider is available only when a compatible harness exists and provider config plus a usable credential are present. Installer artifacts are hints only; they do not make a provider available by themselves.

## Declared providers

| Provider | Harness | `base_url` | `env_key` | `wire_api` | Probe |
| --- | --- | --- | --- | --- | --- |
| Fugu / Sakana | `codex` | `https://api.sakana.ai/v1` | `SAKANA_API_KEY` | `responses` | `/models` |

Fugu is provisioned per agent through an isolated `CODEX_HOME` (`~/.agend-fugu-codex`) and a per-instance `env.CODEX_HOME` in `fleet.yaml`. This avoids mutating the operator's global `~/.codex` when Agend-Terminal creates a Fugu pane.

Endpoint probes are optional, cached, and fail-open. Startup/menu detection must not depend on live network calls; a failed or anomalous probe is reported as `unknown`, not as provider absence.

## Fixed-provider backends

Some CLI backends are deliberately outside the provider axis:

- `kiro-cli` — fixed AWS endpoint / signed-auth shape, not a bearer `base_url` override.
- `agy` — Google service-account/OAuth shape, not a bearer `base_url` override.

They remain normal PATH-detected backends rather than provider-swappable harnesses.

## Diagnostics

Run:

```sh
agend-terminal doctor providers
agend-terminal doctor providers --format json
agend-terminal doctor providers --probe
```

The diagnostics show Fugu's tri-state availability, the descriptor fields, any model catalog entries found locally, and the fixed-provider backend boundary.
