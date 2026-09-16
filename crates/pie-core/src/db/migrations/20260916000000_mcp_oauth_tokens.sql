-- Durable OAuth 2.1 credentials for MCP servers ([mcp.<name>.auth]).
-- `pie mcp login` stores the token set here; runs authorize from it and
-- refresh transparently. One row per server; `credentials` is the JSON of
-- rmcp's StoredCredentials (token, granted scopes, issuer, client id).

CREATE TABLE IF NOT EXISTS mcp_oauth_tokens (
    server_name TEXT PRIMARY KEY,
    credentials TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
