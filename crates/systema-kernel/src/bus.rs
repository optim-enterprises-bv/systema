//! Local message bus between agents in one kernel. Messages use the same A2A-shaped
//! [`Message`] type as the network, so an agent can move to another node unchanged.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use systema_proto::Message;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
pub struct Bus {
    routes: Arc<RwLock<HashMap<String, mpsc::Sender<Message>>>>,
}

#[derive(Debug)]
pub enum BusError {
    NoSuchAgent(String),
    Closed(String),
}

impl std::error::Error for BusError {}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusError::NoSuchAgent(a) => write!(f, "no agent named {a}"),
            BusError::Closed(a) => write!(f, "agent {a} is not receiving"),
        }
    }
}

impl Bus {
    /// Register an agent mailbox. Re-registering replaces the old one.
    pub fn register(&self, agent: &str) -> mpsc::Receiver<Message> {
        let (tx, rx) = mpsc::channel(64);
        self.routes.write().unwrap().insert(agent.to_string(), tx);
        rx
    }

    pub fn unregister(&self, agent: &str) {
        self.routes.write().unwrap().remove(agent);
    }

    pub fn agents(&self) -> Vec<String> {
        let mut v: Vec<_> = self.routes.read().unwrap().keys().cloned().collect();
        v.sort();
        v
    }

    /// Deliver to a local agent. Remote addresses (`agent@peer`) are the node's job.
    pub async fn send(&self, msg: Message) -> Result<(), BusError> {
        let to = msg.to.split('@').next().unwrap_or("").to_string();
        let tx = self.routes.read().unwrap().get(&to).cloned().ok_or_else(|| BusError::NoSuchAgent(to.clone()))?;
        tx.send(msg).await.map_err(|_| BusError::Closed(to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn routes_by_name() {
        let bus = Bus::default();
        let mut rx = bus.register("sysadmin");
        bus.send(Message::text("coder", "sysadmin", "disk usage?")).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().first_text(), Some("disk usage?"));
        assert!(matches!(bus.send(Message::text("coder", "nobody", "x")).await, Err(BusError::NoSuchAgent(_))));
    }
}
