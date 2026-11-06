# rmcp for the MCP layer

We use the official Rust MCP SDK (`rmcp`) with its streamable-HTTP server mounted in axum, instead of a hand-written transport. It is actively maintained and tracks the spec; owning the protocol ourselves buys nothing for a read-only tool server. Our OAuth authorization server stays our own code in front of the MCP endpoint (the SDK only sees already-authenticated requests). Revisit only if rmcp blocks the auth integration.
