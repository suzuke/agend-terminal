# Telegram Inbound Attachment Support (Photo + Document)

**Branch:** `telegram-attachments`
**Date:** 2026-04-22

---

## Problem

`handle_message()` in `telegram.rs` early-returns on `msg.text() == None`, silently dropping photo and document messages.

## Design

### Data Flow

```
Telegram photo/doc msg
  │
  ▼
handle_message()
  ├─ msg.text()     → (text, None)              ← existing path, unchanged
  ├─ msg.photo()    → (caption, Some(file_id))  ← largest PhotoSize
  ├─ msg.document() → (caption, Some(file_id))
  └─ else           → return                     ← sticker, voice, etc.
  │
  ▼ (if attachment present)
try_download_attachment_to(file_id, $AGEND_HOME/downloads/{instance}/)
  │
  ▼
Append "\n[attachment: {file_id} -> {local_path}]" to text
  │
  ▼
Existing routing: raw keystrokes or inbox enqueue (unchanged)
```

### Changes to `handle_message()` (telegram.rs ~line 241)

Replace:
```rust
let text = match msg.text() { Some(t) => t, None => return };
```

With extraction of `(text: String, attachment: Option<String>)`:
- `msg.text()` → text-only, no attachment
- `msg.photo()` → `sizes.last().file.id` + `msg.caption()`
- `msg.document()` → `doc.file.id` + `msg.caption()`
- else → return (unchanged)

After topic resolution, if `attachment.is_some()`:
1. Download via `try_download_attachment_to()` to `$AGEND_HOME/downloads/{instance}/`
2. Append attachment info to text (success or failure)
3. Continue into existing routing

### New helper function

```rust
fn try_download_attachment_to(home: &Path, file_id: &str, dest_dir: &Path) -> Result<String>
```

Reuses `resolve_channel_only()` + `telegram_runtime()` pattern. Distinct from the existing `try_download_attachment()` (MCP tool, takes `instance_name`) to avoid changing the public API.

## Files to Modify

| File | Change |
|------|--------|
| `src/telegram.rs` | Modify `handle_message()` extraction logic; add `try_download_attachment_to()` |

Single file change. No trait/config/fleet.yaml changes needed.

## Bug in Existing Commit (a21d9c9)

The commit has a duplicate `let sender_id` binding (line 249-250). The second shadows the first harmlessly but should be removed.

## Constraints

- Text-only messages: zero behavior change
- Download failure: non-fatal — append `(download failed)` and continue
- Telegram file size limit: Bot API allows up to 20MB download; larger files will fail gracefully
