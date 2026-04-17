#!/usr/bin/env bash
# Verify the code-review fixes from Round 1-4.
# Exit 0 if all checks pass; non-zero on first failure.
set -euo pipefail

cd "$(dirname "$0")/.."

red()   { printf '\033[31m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
info()  { printf '\033[36m[check]\033[0m %s\n' "$*"; }

fail() { red "FAIL: $*"; exit 1; }

# ---------------------------------------------------------------------------
# Round 2: no raw lock().unwrap() in CLI / verify paths
# ---------------------------------------------------------------------------
info "Round 2 — no raw lock().unwrap() in cli.rs / verify.rs"
if grep -n "\.lock()\.unwrap()" src/cli.rs src/verify.rs; then
    fail "found raw lock().unwrap() — use .unwrap_or_else(|e| e.into_inner())"
fi
green "  ok"

# ---------------------------------------------------------------------------
# Round 4: no stale comments
# ---------------------------------------------------------------------------
info "Round 4 — no 'workspaces' comment drift, no kiro debug marker"
# Only flag 'workspaces' in comments (not variable names like `let workspaces = ...`)
if grep -rn "//.*workspaces\|/\*.*workspaces" src/; then
    fail "'workspaces' (plural) found in a comment — should be 'workspace'"
fi
if grep -rn "remove after fixing kiro" src/; then
    fail "kiro debug marker still present"
fi
green "  ok"

# ---------------------------------------------------------------------------
# Round 1: team spawn pre-generates configs before API call
# Checks the structural ordering in src/mcp/handlers.rs:
#   the `for i in 1..=count` loop that calls instructions::generate
#   must appear BEFORE `crate::api::call` with method::CREATE_TEAM.
# ---------------------------------------------------------------------------
info "Round 1 — team spawn instructions generated before API call"
PRE_LINE=$(grep -n "instructions::generate" src/mcp/handlers.rs | head -1 | cut -d: -f1)
API_LINE=$(grep -n "method::CREATE_TEAM" src/mcp/handlers.rs | head -1 | cut -d: -f1)
if [[ -z "$PRE_LINE" || -z "$API_LINE" ]]; then
    fail "expected markers not found in src/mcp/handlers.rs"
fi
if (( PRE_LINE >= API_LINE )); then
    fail "instructions::generate at line $PRE_LINE is NOT before CREATE_TEAM at line $API_LINE"
fi
green "  ok (generate@L$PRE_LINE < create_team@L$API_LINE)"

# ---------------------------------------------------------------------------
# Round 3: handle_instance_created only calls attach_pane once
# ---------------------------------------------------------------------------
info "Round 3 — handle_instance_created calls attach_pane exactly once"
# Extract function body and count attach_pane calls
BODY=$(awk '/^fn handle_instance_created\(/,/^}$/' src/app.rs)
COUNT=$(echo "$BODY" | grep -c "attach_pane(" || true)
if (( COUNT != 1 )); then
    fail "handle_instance_created has $COUNT attach_pane calls, expected 1"
fi
green "  ok (1 attach_pane call)"

# ---------------------------------------------------------------------------
# Round A: layout bounds + unicode width
# ---------------------------------------------------------------------------
info "Round A — MIN_PANE_CELLS + ratio_bounds present in layout.rs"
if ! grep -q "const MIN_PANE_CELLS" src/layout.rs; then
    fail "MIN_PANE_CELLS constant missing from src/layout.rs"
fi
if ! grep -q "fn ratio_bounds" src/layout.rs; then
    fail "ratio_bounds helper missing from src/layout.rs"
fi
if ! grep -q "UnicodeWidthStr::width" src/layout.rs; then
    fail "title bar sizing must use UnicodeWidthStr::width (CJK/emoji width)"
fi
green "  ok"

info "Round A — tests cover ratio_bounds + unicode title width"
for t in ratio_bounds_symmetric_when_room \
         ratio_bounds_degenerate_when_tiny \
         ratio_to_size_no_zero_when_room \
         unicode_width_for_title_matches_terminal_cells; do
    if ! grep -q "fn $t" src/layout.rs; then
        fail "missing regression test: $t"
    fi
done
green "  ok"

# ---------------------------------------------------------------------------
# Round B: selection cache merged + tab switch clears transient state
# ---------------------------------------------------------------------------
info "Round B — clear_selection_cache merged into handle_mouse_selection"
if grep -qn "fn clear_selection_cache" src/app.rs; then
    fail "clear_selection_cache should have been merged into handle_mouse_selection"
fi
if grep -qn "clear_selection_cache(" src/app.rs; then
    fail "clear_selection_cache is still being called (should be merged)"
fi
green "  ok"

info "Round B — Layout::switch_active centralizes tab-switch state clearing"
if ! grep -q "fn switch_active" src/layout.rs; then
    fail "Layout::switch_active helper missing"
fi
if ! grep -q "fn clear_transient_input" src/layout.rs; then
    fail "Tab::clear_transient_input helper missing"
fi
# Only one `self.active =` assignment should remain (inside switch_active itself).
ACTIVE_ASSIGNS=$(grep -c "self\.active = " src/layout.rs || true)
if (( ACTIVE_ASSIGNS != 1 )); then
    fail "expected exactly 1 'self.active =' assignment in layout.rs (inside switch_active); found $ACTIVE_ASSIGNS"
fi
green "  ok (1 self.active assignment, inside switch_active)"

# ---------------------------------------------------------------------------
# Round C: overlay modal + drag guard + zoom blocks border hit-test
# ---------------------------------------------------------------------------
info "Round C — overlay swallows mouse events"
if ! grep -q "Event::Mouse(_) if !matches!(overlay, Overlay::None)" src/app.rs; then
    fail "overlay-modal guard for Event::Mouse missing in src/app.rs"
fi
green "  ok"

info "Round C — drag-to-swap requires pane_count > 1"
# The guard and the assignment must both appear. Use a window grep.
if ! grep -A 3 "tab.root().pane_count() > 1" src/app.rs | grep -q "dragging_pane = Some"; then
    fail "drag-to-swap must be gated by pane_count() > 1 in src/app.rs"
fi
green "  ok"

info "Round C — zoomed mode skips find_split_border"
# In the mouse-Down non-title branch, border detection must be guarded by !zoomed.
if ! grep -q "else if !zoomed" src/app.rs; then
    fail "find_split_border branch should be gated by !zoomed in src/app.rs"
fi
green "  ok"

# ---------------------------------------------------------------------------
# Round D: drag border distinct from state colors + help lists every command
# ---------------------------------------------------------------------------
info "Round D — drag borders use REVERSED modifier (distinct from state colors)"
# The drag branch must apply Modifier::REVERSED so Magenta-drag-source and
# Green-drag-target aren't confused with Magenta=PermissionPrompt / Green=Ready.
if ! grep -A 4 "is_drag_source" src/render.rs | grep -q "Modifier::REVERSED"; then
    fail "drag source border must use Modifier::REVERSED"
fi
if ! grep -A 4 "is_drag_target" src/render.rs | grep -q "Modifier::REVERSED"; then
    fail "drag target border must use Modifier::REVERSED"
fi
green "  ok"

info "Round D — help text lists every palette command implemented in execute_command"
# Extract command names from match arms inside fn execute_command.
# Only match lines that look like pattern arms: `"<name>"[ | "<name>"]* => {`
# (excludes quoted strings used inside arm bodies, e.g. "claude", "\r").
BODY=$(awk '/^fn execute_command\(/,/^}$/' src/app.rs)
CMDS=$(echo "$BODY" \
    | grep -E '^[[:space:]]*"[a-z]+"([[:space:]]*\|[[:space:]]*"[a-z]+")*[[:space:]]*=>' \
    | grep -oE '"[a-z]+"' \
    | tr -d '"' \
    | sort -u)
if [[ -z "$CMDS" ]]; then
    fail "could not extract palette commands from execute_command"
fi
MISSING=""
for cmd in $CMDS; do
    if ! grep -q ":$cmd" src/render.rs; then
        MISSING="$MISSING $cmd"
    fi
done
if [[ -n "$MISSING" ]]; then
    fail "help text is missing these palette commands:$MISSING"
fi
green "  ok ($(echo $CMDS | wc -w | tr -d ' ') commands, all documented)"

# ---------------------------------------------------------------------------
# Round E: interactive polish from real-run feedback
#   (1) drag smoothness (defer PTY resize to mouse-up)
#   (2) no phantom '_' in input overlays
#   (3) portable H/J/K/L resize keys
# ---------------------------------------------------------------------------
info "Round E — border drag defers PTY resize until mouse-up"
# The Drag branch for border_drag must NOT set needs_resize = true; the Up
# branch for border_drag must set it. Verify both structurally.
DRAG_BLOCK=$(awk '/if let Some\(\(ref hit, ref pa\)\) = border_drag \{/,/\}[[:space:]]*else if layout\.active_tab/' src/app.rs | head -40)
if echo "$DRAG_BLOCK" | grep -q "needs_resize = true"; then
    fail "border_drag Drag branch must not set needs_resize (causes PTY thrash)"
fi
# Up branch: first clause after border_drag.is_some() should set needs_resize.
UP_BLOCK=$(awk '/if border_drag\.is_some\(\) \{/,/\} else if layout\.active_tab\(\)\.is_some_and\(\|t\| t\.dragging_pane/' src/app.rs)
if ! echo "$UP_BLOCK" | grep -q "needs_resize = true"; then
    fail "border_drag Up branch must set needs_resize = true (deferred resize)"
fi
green "  ok"

info "Round E — no phantom '_' in Command / Rename overlays"
# The literal format!("...{input}_") pattern was leaking as a visual character
# alongside the real terminal cursor.
if grep -nE 'format!\("[^"]*\{input\}_"' src/render.rs; then
    fail "render.rs still has format!(\"...{input}_\") that prints a phantom underscore"
fi
green "  ok"

info "Round E — portable H/J/K/L resize fallback present"
for k in "KeyCode::Char('H') => Action::ResizeLeft" \
         "KeyCode::Char('J') => Action::ResizeDown" \
         "KeyCode::Char('K') => Action::ResizeUp" \
         "KeyCode::Char('L') => Action::ResizeRight"; do
    if ! grep -qF "$k" src/keybinds.rs; then
        fail "missing portable resize binding: $k"
    fi
done
green "  ok"

# ---------------------------------------------------------------------------
# Build + tests
# ---------------------------------------------------------------------------
info "cargo build"
cargo build --quiet 2>&1 | tail -5
green "  ok"

info "cargo test --bin agend-terminal"
cargo test --bin agend-terminal --quiet 2>&1 | tail -3
green "  ok"

echo
green "All review-fix checks passed."
