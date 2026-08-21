# Use the official async MCP SDK and a loopback dashboard

Crusty uses the official Rust MCP SDK on Tokio so independent requests remain responsive while blocking repository work runs outside the async executor; operations expected to exceed two seconds return durable task IDs with progress and cancellation. Human review is presented through an authenticated loopback-only Axum dashboard with a one-time bootstrap URL, SameSite session cookie, CSRF validation, and a restrictive content-security policy. This replaces the hand-written synchronous protocol loop while keeping the product local.
