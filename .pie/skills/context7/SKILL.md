---
name: context7
description: Fetch library documentation via Context7 API — search for a library, then fetch version-specific docs and code examples.
---

## Context7 API

Context7 provides version-specific documentation and code examples for libraries
and frameworks.

### Step 1: Search for a library

```bash
curl -s 'https://context7.com/api/v1/search?query=LIBRARY_NAME'
```

Parse the JSON response. Pick the best match from `results` — use the `id` field
(without the leading `/`) as the library slug. Prefer results with high `stars`,
`trustScore`, and `benchmarkScore`. If ambiguous, pick the most popular result.

### Step 2: Fetch documentation

```bash
curl -s 'https://context7.com/api/v1/OWNER/REPO?topic=TOPIC'
```

- `OWNER/REPO` — the slug from step 1 (e.g. `facebook/react`, `tokio-rs/tokio`,
  `n0-computer/iroh`)
- `topic` — specific feature or concept (optional; omit for general overview)

If you see `has been redirected to` in the response, use the redirected slug
instead and retry.

### Error handling

| Response                     | Meaning                     | Action                              |
| ---------------------------- | --------------------------- | ----------------------------------- |
| `{"error":"invalid_format"}` | Missing `owner/repo` format | Fix slug to `owner/repo`            |
| `Library ... not found`      | Wrong slug                  | Try alternative from search results |
| `has been redirected to`     | Library moved               | Use the new slug from the message   |
| Empty search results         | Library not indexed         | Report and try alternative sources  |

### Rules

- ALWAYS search first (step 1) to get the correct slug — never guess
- Pass a `topic` parameter when the user asks about a specific feature
- For multi-library queries, search and fetch each separately
