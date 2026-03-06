---
name: browser-automation
description: Control a headless Chrome browser via Pinchtab's REST API
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
priority: 50
---

## Browser Automation (Pinchtab)

Use the `browser` tool for common operations. It handles tab locking,
cleanup, and accessibility tree wait times automatically.

### Quick Start

1. **Navigate** (auto-acquires tab, waits 3s, returns snapshot):
   `browser(action="navigate", url="https://example.com")`

2. **Act** on a ref from the snapshot:
   `browser(action="act", kind="click", ref="e5", tabId="<from step 1>")`

3. **Read text** (~800 tokens vs ~10K for full snapshot):
   `browser(action="text", tabId="...")`

4. **Diff snapshot** (only changes, ~90% fewer tokens):
   `browser(action="snapshot", tabId="...", diff=true)`

5. **List tabs** (see lock status):
   `browser(action="tabs")`

### Core Loop

1. **Navigate** → opens URL, auto-acquires + locks a tab, returns snapshot
2. **Act** → click/type/fill/press using refs from the snapshot
3. **Snapshot** → use `diff=true` to see only changes (~90% fewer tokens)
4. Repeat 2–3 until done

### Tab Management

- The tool acquires and locks a tab automatically on each call
- Pass `tabId` from a previous result to reuse the same tab
- The tool unlocks the tab when the operation completes (even on failure)
- **20-tab system limit** — the tool handles capacity automatically
- Locks auto-expire after timeout; if your tab is stolen, the tool
  re-acquires from available tabs

### Navigate vs Click — Critical Rule

**`navigate` is for going to URLs. `click` is for interactions with no URL.**

When following a link, **never click it** — extract the URL and navigate directly.
Clicking triggers hover states, preview popovers, and JavaScript event handlers that
often intercept navigation (search result previews, HackerNews/Reddit card expansions,
SPAs with custom routing, sticky overlays). Direct navigation bypasses all of this.

```
# WRONG — triggers Google preview, HN popover, etc.
browser(action="act", kind="click", ref="e5", tabId="...")

# RIGHT — extract href from snapshot or evaluate, then navigate
browser(action="evaluate", expression="document.querySelector('a.result').href", tabId="...")
browser(action="navigate", url="<extracted url>")
```

If the snapshot already shows the URL on a link ref, navigate to it directly without
touching the DOM at all. Use `click` only when there is no URL involved: buttons,
form submissions, toggles, checkboxes, dropdowns, and in-page UI controls.

### Action Kinds

For `action="act"`, use these `kind` values:
- `click` — click element by ref (add `waitNav=true` if it triggers navigation)
- `type` — type text into element (set `text`)
- `press` — press a key (set `key`, e.g. "Enter")
- `fill` — set value directly (set `text` and optionally `selector`)
- `hover` — hover over element (triggers dropdowns/tooltips)
- `scroll` — scroll to element by ref
- `select` — select dropdown option (set `value`)
- `focus` — focus an element

### Key Endpoints

All endpoints use flat paths. Multi-tab targeting uses `?tabId=ID` query parameter or `"tabId"` in POST body.

| Endpoint | Method | Purpose |
|---|---|---|
| `/navigate` | POST | Navigate URL → `{tabId, url, title}` |
| `/tabs` | GET | List all tabs → `{tabs: [{id, title, url, type}]}` |
| `/tab` | POST | `{"action":"new","url":"..."}` or `{"action":"close","tabId":"..."}` |
| `/snapshot` | GET | Accessibility tree. Params: `tabId`, `filter=interactive`, `diff=true`, `maxTokens=2000`, `format=compact` |
| `/text` | GET | Readable text. Params: `tabId`, `mode=raw` |
| `/action` | POST | `{"kind":"click\|type\|fill\|press\|hover\|scroll\|select\|focus", "ref":"e5", "tabId":"..."}` |
| `/actions` | POST | Batch: `{"actions":[...], "stopOnError":true, "tabId":"..."}` |
| `/screenshot` | GET | Binary PNG. Params: `tabId`, `raw=true` → save with `curl -o /shared/file.png` |
| `/pdf` | GET | `?tabId=...&output=file&path=/shared/.pinchtab/page.pdf` |
| `/evaluate` | POST | Run JS: `{"expression":"document.title", "tabId":"..."}` |
| `/cookies` | GET/POST | Get/set cookies. Param: `tabId` |
| `/download` | GET | Download file: `?url=...&output=file&path=/shared/.pinchtab/file.ext` |
| `/upload` | POST | Upload: `{"selector":"input[type=file]","paths":["/shared/file.jpg"],"tabId":"..."}` |
| `/health` | GET | Health check → `{status, tabs}` |

### Best Practices

- Always use `maxTokens=2000` on snapshots (full trees can exceed 10K tokens)
- Use `filter=interactive` to see only clickable/input elements
- Use `diff=true` after actions for ~90% token savings
- Use `/text` for reading content (~800 tokens/page)
- Use `format=compact` for most token-efficient snapshots
- Batch interactions with `POST /actions` for fewer round-trips

### File Operations (use shell_exec + curl)

Save screenshots:
  `curl "{{PINCHTAB_URL}}/screenshot?tabId=$TAB&raw=true" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" -o /shared/screenshot.png`

Export PDF:
  `curl "{{PINCHTAB_URL}}/pdf?tabId=$TAB&output=file&path=/shared/.pinchtab/page.pdf" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" && \
    cp /shared/.pinchtab/page.pdf /shared/page.pdf`

Download file:
  `curl "{{PINCHTAB_URL}}/download?url=URL&output=file&path=/shared/.pinchtab/f.ext" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" && \
    cp /shared/.pinchtab/f.ext /shared/f.ext`

Upload file:
  `curl -X POST "{{PINCHTAB_URL}}/upload" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"selector":"input[type=file]","paths":["/shared/photo.jpg"],"tabId":"ID"}'`

Evaluate JS:
  `curl -X POST "{{PINCHTAB_URL}}/evaluate" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"expression":"document.title","tabId":"ID"}'`

Set cookies:
  `curl -X POST "{{PINCHTAB_URL}}/cookies" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"url":"https://example.com","cookies":[{"name":"s","value":"v"}]}'`

Stealth/fingerprint:
  `curl "{{PINCHTAB_URL}}/stealth/status" -H "Authorization: Bearer $BRIDGE_TOKEN"`
  `curl -X POST "{{PINCHTAB_URL}}/fingerprint/rotate" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" \
    -H 'Content-Type: application/json' -d '{"os":"windows"}'`

After saving files to /shared/, use `send_media` to deliver to the user.

### If Blocked

If you encounter CAPTCHAs, 2FA, or login walls, call `request_human_assistance`
with a clear description of the blocker.

Full API reference:
`cat /shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md`
