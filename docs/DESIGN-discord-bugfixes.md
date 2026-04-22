# Discord Bugfixes: Attachment Download + Channel Routing

**Date:** 2026-04-22

---

## Bug 1: Discord Attachment Download Fails ("builder error")

### Root Cause

The MCP `download_attachment` tool passes `file_id` (which is the attachment filename, e.g. `8719DE7B.png`) to `try_discord_download()`, which calls `reqwest::get(url)` on it. A filename is not a URL → reqwest builder error.

Meanwhile, the inbound handler (line ~385) already downloads attachments correctly via `att.download().await` and saves them to `$AGEND_HOME/downloads/{instance}/`.

### Fix: Return Local Path (Scheme A)

Since inbound already downloads the file, `try_discord_download` should just return the existing local path:

```rust
pub fn try_discord_download(instance_name: &str, file_id: &str) -> anyhow::Result<String> {
    let home = crate::home_dir();
    let path = home.join("downloads").join(instance_name).join(file_id);
    if path.exists() {
        Ok(path.display().to_string())
    } else {
        anyhow::bail!("attachment not found: {}", path.display())
    }
}
```

This is correct because:
- Discord CDN URLs expire, so re-downloading later would fail anyway
- Inbound handler already saves with `att.filename` as the filename
- The `[attachment: {filename}]` text appended to the message matches the saved filename
- Matches the semantic contract: "give me the local path to this attachment"

### File Change

| File | Change |
|------|--------|
| `src/discord.rs` | Replace `try_discord_download()` body: check local path exists, return it |

---

## Bug 2: Non-AgEnD Channel Messages Routed to General

### Root Cause

`resolve_channel_to_instance()` falls back to `"general"` for any unknown `channel_id`. The bot can read messages from all channels it has permission for (e.g. the server's default `#general`), so those messages get incorrectly routed to the AgEnD general instance.

### Fix: Ignore Messages from Unregistered Channels

Add a check in `EventHandler::message` before routing. Two options:

**Option A (simple, recommended):** Check if `channel_id` is in the registered map. If not, ignore.

```rust
// In EventHandler::message, after extracting channel_id:
let instance_name = {
    let mut s = lock_state(&self.state);
    let cid = msg.channel_id.get();
    if !s.channel_to_instance.contains_key(&cid) {
        // Reload from fleet.yaml (same as current logic)
        // If still not found → return (ignore), don't fallback to "general"
        return;
    }
    resolve_channel_to_instance(&mut s, cid)
};
```

**Option B (category check):** Verify the channel's `parent_id` matches the AgEnD category. More robust but requires an extra API call or cache lookup.

**Recommendation: Option A.** It's simpler, no extra API calls, and the channel map is the source of truth — if a channel isn't registered, it's not an AgEnD channel.

### Change to `resolve_channel_to_instance()`

Change the fallback from `"general".to_string()` to `None`:

```rust
fn resolve_channel_to_instance(state: &mut DiscordState, channel_id: u64) -> Option<String> {
    // ... existing lookup + fleet.yaml reload ...
    // If still not found:
    None  // was: "general".to_string()
}
```

Caller checks `None` → ignore message silently.

### File Change

| File | Change |
|------|--------|
| `src/discord.rs` | Change `resolve_channel_to_instance` return type to `Option<String>`, return `None` instead of `"general"` fallback. Update `EventHandler::message` to skip on `None`. |

---

## Summary

| Bug | Fix | Files | Risk |
|-----|-----|-------|------|
| Attachment download | Return local path instead of re-downloading | `discord.rs` | Zero — inbound already downloads correctly |
| Channel routing | Return `None` for unregistered channels, skip message | `discord.rs` | Low — only changes fallback behavior for non-AgEnD channels |

Both fixes are in `src/discord.rs` only.
