//! MCP server: protocol handling, tools, resources, subscriptions.

#[allow(dead_code)] // complete JSON-RPC 2.0 types; not every helper is used
pub mod jsonrpc;
pub mod protocol;
pub mod request_state;
pub mod session;
pub mod subscriptions;
mod tools;

#[cfg(test)]
pub(crate) mod tests;

pub use session::McpSession;
pub use subscriptions::SessionReply;
