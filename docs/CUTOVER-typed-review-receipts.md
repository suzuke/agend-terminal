# Typed review-receipt cutover

Task66 is a containment boundary, not the task68 append-only verdict ledger.
After this release, only a server-validated `code_review` report tied to an
exact active assignment can affect PR state, review bridging, auto-release, or
assignment evidence. Existing assignment rows are never inferred or upgraded.

## Required pre-enable census

Pause new reviewer-assignment dispatches and take a backup of
`$AGEND_HOME/reviewer-assignments`. Then run this inventory with Bash and `jq`:

```bash
legacy=0
while IFS= read -r -d '' row; do
  item="$({
    jq -c --arg path "$row" '
      if type != "object" or (.assignment_id? | type) != "string" then
        error("corrupt assignment row: " + $path)
      elif
        ((.schema_version // 1) < 2)
        or (.target_instance_id? == null)
        or (((.reviewed_head // "") | test("^([0-9A-Fa-f]{40}|[0-9A-Fa-f]{64})$")) | not)
        or ((.review_slot // "") != "primary" and (.review_slot // "") != "secondary")
        or (((.review_class // "Unresolved") | ascii_downcase) == "unresolved")
      then
        {
          path: $path,
          assignment_id,
          repo,
          branch,
          pr_number,
          task_id,
          target,
          action: "audited re-dispatch required"
        }
      else empty end
    ' "$row"
  })" || exit 2
  if [[ -n "$item" ]]; then
    printf '%s\n' "$item"
    legacy=$((legacy + 1))
  fi
done < <(
  find "${AGEND_HOME:?set AGEND_HOME}/reviewer-assignments" \
    -type f -name '*.json' ! -name markers.json -print0
)
((legacy == 0))
```

Exit `0` means every parseable active row has the typed subject fields. Exit
`1` prints LegacyAssignments that require re-dispatch. Exit `2` means the
inventory is unreadable or corrupt; stop the rollout and repair it rather than
treating it as empty.

For every listed non-terminal generation, re-dispatch through the normal
`review_assignment` path with the same task, reviewer, repository, branch, and
PR number plus the current exact 40/64-hex head. Preserve the task's explicit
`single`/`dual` review class and primary/secondary role. Do not edit JSON rows or
copy caller-supplied receipt IDs. The durable dispatch mints a new assignment
generation, captures the reviewer's stable InstanceId, and supersedes the old
row at the same assignment key.

Run the census again from a quiescent snapshot. Enable the release only after it
returns `0`. On first daemon reconciliation, task66 also performs a strict
one-time census: it logs every remaining LegacyAssignment and reports an error
if any row is unreadable. Any missed legacy row stays fail-closed and cannot
submit a review receipt.

## Post-enable checks

- Send one ordinary `analysis_decision` report beginning with `VERIFIED`; it
  must remain an ordinary report with no PR-state, buffer, bridge, release, or
  assignment-evidence mutation.
- Exercise one exact typed review receipt and verify its `source_id` is the
  server message ID and its `receipt_id` is derived from that ID.
- Revoke an assignment before a buffered receipt replays; the receipt must be
  consumed as inert and must not satisfy the replacement generation.
- Do not claim task68 properties here: this cutover does not add an append-only
  verdict ledger, worst-verdict reduction, or monotonic revocation history.
