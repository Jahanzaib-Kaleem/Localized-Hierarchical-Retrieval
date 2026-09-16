# MCP/OpenAI compatibility notes

This note records the protocol assumptions used by the LHR MCP control plane so they remain explicit during future upgrades.

- Modern transport target: MCP `2026-07-28`, stateless HTTP request/response.
- Compatibility target: legacy `2025-11-25` initialize-era clients.
- Modern requests validate `MCP-Protocol-Version` and `Mcp-Method`; `tools/call` also validates `Mcp-Name` against `params.name`.
- Modern `tools/list` responses include `ttlMs` and `cacheScope`.
- Modern responses include server identity in `_meta["io.modelcontextprotocol/serverInfo"]`.
- No MCP protocol session is required for the modern path.
- LHR does not require server-to-client sampling, elicitation, subscriptions, or other held-open protocol state.
- Remote clients authenticate with the same bearer-key role model as the HTTP API.

These are wire/protocol compatibility notes, not an authorization substitute. LHR roles and resource ceilings remain authoritative regardless of what an MCP client advertises in `_meta`.
