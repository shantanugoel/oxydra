# Pinchtab API Reference

Base URL for all examples: `http://localhost:9867`

> **Auth:** All requests require `-H "Authorization: Bearer $BRIDGE_TOKEN"`.

## Navigate

```bash
curl -X POST /navigate \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"url": "https://example.com"}'

# With options: custom timeout, block images, open in new tab
curl -X POST /navigate \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"url": "https://example.com", "timeout": 60, "blockImages": true, "newTab": true}'
```

## Snapshot (accessibility tree)

```bash
# Full tree
curl /snapshot -H "Authorization: Bearer $BRIDGE_TOKEN"

# Interactive elements only (buttons, links, inputs) — much smaller
curl "/snapshot?filter=interactive" -H "Authorization: Bearer $BRIDGE_TOKEN"

# Smart diff — only changes since last snapshot (massive token savings)
curl "/snapshot?diff=true" -H "Authorization: Bearer $BRIDGE_TOKEN"

# Compact format — most token-efficient (recommended)
curl "/snapshot?format=compact" -H "Authorization: Bearer $BRIDGE_TOKEN"

# Scope to CSS selector (e.g. main content only)
curl "/snapshot?selector=main" -H "Authorization: Bearer $BRIDGE_TOKEN"

# Truncate to ~N tokens
curl "/snapshot?maxTokens=2000" -H "Authorization: Bearer $BRIDGE_TOKEN"

# Combine for maximum efficiency
curl "/snapshot?format=compact&selector=main&maxTokens=2000&filter=interactive" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Disable animations before capture
curl "/snapshot?noAnimations=true" -H "Authorization: Bearer $BRIDGE_TOKEN"
```

Returns flat JSON array of nodes with `ref`, `role`, `name`, `depth`, `value`, `nodeId`.

**Token optimization**: Use `?format=compact` for best token efficiency. Add `?filter=interactive` for action-oriented tasks (~75% fewer nodes). Use `?selector=main` to scope to relevant content. Use `?maxTokens=2000` to cap output. Use `?diff=true` on multi-step workflows to see only changes.

## Act on elements

```bash
# Click by ref
curl -X POST /action -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "click", "ref": "e5"}'

# Type into focused element
curl -X POST /action -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "type", "ref": "e12", "text": "hello world"}'

# Press a key
curl -X POST /action -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "press", "key": "Enter"}'

# Fill (set value directly, no keystrokes)
curl -X POST /action -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "fill", "selector": "#email", "text": "user@example.com"}'

# Hover (trigger dropdowns/tooltips)
curl -X POST /action -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "hover", "ref": "e8"}'

# Select dropdown option
curl -X POST /action -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "select", "ref": "e10", "value": "option2"}'

# Scroll by pixels (infinite scroll pages)
curl -X POST /action -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "scroll", "scrollY": 800}'

# Click and wait for navigation
curl -X POST /action -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "click", "ref": "e5", "waitNav": true}'
```

## Batch actions

```bash
curl -X POST /actions -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"actions":[{"kind":"click","ref":"e3"},{"kind":"type","ref":"e3","text":"hello"},{"kind":"press","key":"Enter"}],"stopOnError":true}'
```

## Extract text

```bash
# Readability mode (strips nav/footer/ads)
curl /text -H "Authorization: Bearer $BRIDGE_TOKEN"

# Raw innerText
curl "/text?mode=raw" -H "Authorization: Bearer $BRIDGE_TOKEN"
```

Returns `{url, title, text}`. Cheapest option (~1K tokens for most pages).

## PDF export

```bash
# Save to disk
curl "/tabs/TAB_ID/pdf?output=file&path=/shared/page.pdf" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Raw PDF bytes
curl "/tabs/TAB_ID/pdf?raw=true" -H "Authorization: Bearer $BRIDGE_TOKEN" -o page.pdf

# Landscape with custom scale
curl "/tabs/TAB_ID/pdf?landscape=true&scale=0.8&raw=true" \
  -H "Authorization: Bearer $BRIDGE_TOKEN" -o page.pdf
```

**Query Parameters:** `paperWidth`, `paperHeight`, `landscape`, `marginTop/Bottom/Left/Right`, `scale` (0.1–2.0), `pageRanges`, `displayHeaderFooter`, `headerTemplate`, `footerTemplate`, `preferCSSPageSize`, `generateTaggedPDF`, `generateDocumentOutline`, `output` (file/JSON), `path`, `raw`.

## Download files

```bash
# Save directly to disk (uses browser session/cookies/stealth)
curl "/download?url=https://site.com/export.csv&output=file&path=/shared/export.csv" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Raw bytes
curl "/download?url=https://site.com/image.jpg&raw=true" \
  -H "Authorization: Bearer $BRIDGE_TOKEN" -o image.jpg
```

## Upload files

```bash
curl -X POST "/upload?tabId=TAB_ID" -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"selector": "input[type=file]", "paths": ["/shared/photo.jpg"]}'
```

## Screenshot

```bash
curl "/screenshot?raw=true" -H "Authorization: Bearer $BRIDGE_TOKEN" -o screenshot.jpg
curl "/screenshot?raw=true&quality=50" -H "Authorization: Bearer $BRIDGE_TOKEN" -o screenshot.jpg
```

## Evaluate JavaScript

```bash
curl -X POST /evaluate -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"expression": "document.title"}'
```

## Tab management

```bash
# List tabs
curl /tabs -H "Authorization: Bearer $BRIDGE_TOKEN"

# Open new tab
curl -X POST /tab -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"action": "new", "url": "https://example.com"}'

# Close tab
curl -X POST /tab -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"action": "close", "tabId": "TARGET_ID"}'
```

Multi-tab: pass `?tabId=TARGET_ID` to snapshot/screenshot/text, or `"tabId"` in POST body.

## Cookies

```bash
# Get cookies for current page
curl /cookies -H "Authorization: Bearer $BRIDGE_TOKEN"

# Set cookies
curl -X POST /cookies -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","cookies":[{"name":"session","value":"abc123"}]}'
```

## Stealth

```bash
# Check stealth status
curl /stealth/status -H "Authorization: Bearer $BRIDGE_TOKEN"

# Rotate fingerprint
curl -X POST /fingerprint/rotate -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"os":"windows"}'
```

## Health check

```bash
curl /health
```
