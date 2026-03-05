// Pingora-based reverse proxy with automatic TLS.
//
// This module will:
// - Listen on :80 and :443
// - Route requests by Host header → app port
// - Handle TLS termination via Let's Encrypt (ACME)
// - Health-check aware routing
//
// TODO: Implement once process manager and deploy are working end-to-end.
// Pingora integration requires Linux, so this is stubbed for now.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Routing table: domain → upstream port
#[derive(Debug, Clone)]
pub struct RouteTable {
    routes: Arc<RwLock<HashMap<String, u16>>>,
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            routes: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn set(&self, domain: &str, port: u16) {
        self.routes
            .write()
            .expect("route table lock poisoned")
            .insert(domain.to_string(), port);
    }

    pub fn remove(&self, domain: &str) {
        self.routes
            .write()
            .expect("route table lock poisoned")
            .remove(domain);
    }

    pub fn get(&self, domain: &str) -> Option<u16> {
        self.routes
            .read()
            .expect("route table lock poisoned")
            .get(domain)
            .copied()
    }

    pub fn all(&self) -> HashMap<String, u16> {
        self.routes
            .read()
            .expect("route table lock poisoned")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_table_crud() {
        let rt = RouteTable::new();

        rt.set("cyanea.bio", 10001);
        rt.set("archipelag.io", 10002);

        assert_eq!(rt.get("cyanea.bio"), Some(10001));
        assert_eq!(rt.get("archipelag.io"), Some(10002));
        assert_eq!(rt.get("unknown.com"), None);

        rt.set("cyanea.bio", 10003); // update
        assert_eq!(rt.get("cyanea.bio"), Some(10003));

        rt.remove("cyanea.bio");
        assert_eq!(rt.get("cyanea.bio"), None);

        let all = rt.all();
        assert_eq!(all.len(), 1);
        assert_eq!(all["archipelag.io"], 10002);
    }
}
