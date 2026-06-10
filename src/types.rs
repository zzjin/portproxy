use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Route {
    /// Single DNS label, e.g. "sample-web-auth". The proxy matches this against
    /// the first label of the incoming Host header; the rest of the domain is
    /// ignored (upstream Caddy/Nginx decides what gets forwarded here).
    pub hostname: String,
    pub port: u16,
    /// Wrapper process PID owning this route; 0 = static alias (never pruned).
    pub pid: u32,
}
