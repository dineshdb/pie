---
name: context7
description: Fetch version-specific documentation and code examples.
---

## Search for Library
Always search first to identify the correct `owner/repo` slug.
```bash
curl -s 'https://context7.com/api/v1/search?query=<LIBRARY_NAME>'
```

## Fetch Docs/Examples
Use the slug (e.g., `tokio-rs/tokio`) to get specific information.
```bash
curl -s 'https://context7.com/api/v1/OWNER/REPO?topic=<TOPIC>'
```
*Tip: Omit `topic` for a general overview.*

## Handling Results
- **Redirects**: If the response says `has been redirected to`, use the new slug provided.
- **Popularity**: Prefer results with high `stars` and `trustScore`.
- **Accuracy**: Never guess slugs; the search API is the source of truth.
