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

You can control a headless Chrome browser via Pinchtab at `{{PINCHTAB_URL}}`.
All curl commands require the auth header: `-H "Authorization: Bearer $BRIDGE_TOKEN"`
Chrome starts lazily on the first request.

### Core Loop

1. **Navigate** → creates a tab, returns `tabId`
2. **Snapshot** → accessibility tree with clickable refs (e.g., `e5`)
3. **Act** → click/type/fill/press using refs
4. **Snapshot again** → use `diff=true` to see only changes (~90% fewer tokens)
5. Repeat 3-4 until done

```bash
# Navigate (creates new tab)
TAB=$(curl -s -X POST {{PINCHTAB_URL}}/navigate \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com"}' | jq -r '.tabId')

# Snapshot (interactive elements with refs)
curl -s "{{PINCHTAB_URL}}/tabs/$TAB/snapshot?filter=interactive&maxTokens=2000" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Click a ref
curl -s -X POST "{{PINCHTAB_URL}}/tabs/$TAB/action" \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind":"click","ref":"e5"}'

# Diff snapshot (only changes)
curl -s "{{PINCHTAB_URL}}/tabs/$TAB/snapshot?filter=interactive&diff=true" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"
```

### Key Endpoints

| Endpoint | Method | Purpose |
|---|---|---|
| `/navigate` | POST | Navigate URL → `{tabId, url, title}`. Creates new tab if no `tabId` in body |
| `/tabs/{id}/navigate` | POST | Navigate existing tab |
| `/tabs` | GET | List all tabs |
| `/tab` | POST | `{"action":"new"}` or `{"action":"close","tabId":"..."}` |
| `/tabs/{id}/snapshot` | GET | Accessibility tree. Params: `filter=interactive`, `diff=true`, `maxTokens=2000`, `format=compact` |
| `/tabs/{id}/text` | GET | Readable text. Params: `mode=raw`, `maxChars=N` |
| `/tabs/{id}/action` | POST | `{"kind":"click\|type\|fill\|press\|hover\|scroll\|select\|focus", "ref":"e5", ...}` |
| `/tabs/{id}/actions` | POST | Batch: `{"actions":[...], "stopOnError":true}` |
| `/tabs/{id}/screenshot` | GET | Binary PNG → save with `curl -o /shared/file.png` |
| `/tabs/{id}/pdf` | GET/POST | `?output=file&path=/shared/page.pdf` |
| `/tabs/{id}/evaluate` | POST | Run JS: `{"expression":"document.title"}` |
| `/tabs/{id}/cookies` | GET/POST | Get/set cookies |
| `/download` | GET | Download file: `?url=...&output=file&path=/shared/file.ext` |
| `/upload` | POST | Upload file to input: `{"selector":"input[type=file]","paths":["/shared/file.jpg"]}` |

### Best Practices

- Always use `maxTokens=2000` on snapshots (full trees can exceed 10K tokens)
- Use `filter=interactive` to see only clickable/input elements
- Use `diff=true` after actions for ~90% token savings
- Use `/text` for reading content (~800 tokens/page)
- Use `format=compact` for most token-efficient snapshots
- Batch interactions with `POST /tabs/{id}/actions` for fewer round-trips
- Save files to `/shared/` and use `send_media` to deliver to user
- Wait 2-3 seconds after navigation before snapshot for complex pages: `sleep 3`
- Store `$TAB` and reuse it — tab IDs are stable

### File Integration

- Save screenshots: `curl "{{PINCHTAB_URL}}/tabs/$TAB/screenshot" -H "Authorization: Bearer $BRIDGE_TOKEN" -o /shared/screenshot.png`
- Save PDFs: `curl "{{PINCHTAB_URL}}/tabs/$TAB/pdf?output=file&path=/shared/page.pdf" -H "Authorization: Bearer $BRIDGE_TOKEN"`
- Download files: `curl "{{PINCHTAB_URL}}/download?url=...&output=file&path=/shared/report.csv" -H "Authorization: Bearer $BRIDGE_TOKEN"`
- Upload files: First write to /shared/, then `curl -X POST "{{PINCHTAB_URL}}/upload" -H "Authorization: Bearer $BRIDGE_TOKEN" -H 'Content-Type: application/json' -d '{"selector":"input[type=file]","paths":["/shared/file.jpg"]}'`
- Send to user: After saving to /shared/, use `send_media` to deliver the file

### If Blocked

If you encounter CAPTCHAs, 2FA, or login walls, call `request_human_assistance`
with a clear description of the blocker.

For the full API reference (all params, PDF options, upload/download, stealth):
`cat /shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md`
