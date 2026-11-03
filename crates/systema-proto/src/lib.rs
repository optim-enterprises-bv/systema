//! Wire types shared by every Systema component.
//!
//! The shapes follow A2A v1.0 (Agent Card, Message, Part) closely enough that an HTTP
//! A2A gateway can translate 1:1, but Systema carries them inside a [`Signed`] envelope
//! so every card and message is attributable to an ed25519 identity. The same keypair
//! is the node's libp2p `PeerId`, so "who signed this" and "who sent this" are one fact.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use libp2p_identity::{Keypair, PeerId, PublicKey};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub use libp2p_identity;

/// Protocol version advertised in agent cards.
pub const PROTOCOL_VERSION: &str = "systema/0.1 (a2a/1.0)";

/// How long a signed envelope stays acceptable after it was issued.
pub const DEFAULT_MAX_AGE_MS: u64 = 5 * 60 * 1000;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("bad public key encoding")]
    BadKey,
    #[error("bad base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("signature does not verify")]
    BadSignature,
    #[error("envelope signer {signer} does not match claimed peer {claimed}")]
    SignerMismatch { signer: PeerId, claimed: PeerId },
    #[error("envelope is stale or from the future (issued {issued_at_ms}, now {now_ms})")]
    NotFresh { issued_at_ms: u64, now_ms: u64 },
    #[error("signing failed")]
    Signing,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------------------
// Agent cards
// ---------------------------------------------------------------------------------------

/// A capability an agent offers to others, e.g. `code-review`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// One way to reach an agent. A2A v1.0 cards list several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transport {
    /// `libp2p` (multiaddr in `address`) or `https` (A2A JSON-RPC URL).
    pub kind: String,
    pub address: String,
}

/// Public description of an agent, published on the DHT, gossip and `/.well-known/agent-card.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub version: String,
    pub protocol: String,
    /// The node hosting this agent. Must equal the envelope signer.
    pub peer_id: String,
    pub transports: Vec<Transport>,
    pub skills: Vec<Skill>,
    // --- node capabilities (optional; appended so older signed cards still verify byte-for-byte
    // when these are empty/None, because they are skipped from serialization) ---
    /// Model ids this node serves, e.g. ["qwen3.8-27b"]. Lets peers route heavy work to it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Maximum context window in tokens the node can handle, if advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u32>,
    /// Free-text hardware descriptor the operator sets, e.g. "RTX 4090 24GB".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<String>,
}

impl AgentCard {
    /// DHT key under which providers of a skill are recorded.
    pub fn skill_key(skill_id: &str) -> Vec<u8> {
        format!("/systema/skill/{skill_id}").into_bytes()
    }
}

// ---------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Part {
    Text { text: String },
    /// Structured data. BTreeMap keeps serialization deterministic for signing.
    Data { data: BTreeMap<String, serde_json::Value> },
}

/// An A2A-shaped message. `to`/`from` are `agent@peer` addresses (`agent` alone = local).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub from: String,
    pub to: String,
    pub role: Role,
    /// Skill being invoked; the trust gate authorizes per skill.
    pub skill: Option<String>,
    pub task_id: Option<String>,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn text(from: impl Into<String>, to: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: random_id(),
            from: from.into(),
            to: to.into(),
            role: Role::User,
            skill: None,
            task_id: None,
            parts: vec![Part::Text { text: text.into() }],
        }
    }

    pub fn first_text(&self) -> Option<&str> {
        self.parts.iter().find_map(|p| match p {
            Part::Text { text } => Some(text.as_str()),
            _ => None,
        })
    }
}

pub fn random_id() -> String {
    let b: [u8; 12] = rand::random();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ---------------------------------------------------------------------------------------
// Signed envelopes
// ---------------------------------------------------------------------------------------

/// A payload signed by an ed25519 identity, with a nonce and timestamp for replay protection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signed<T> {
    pub payload: T,
    /// libp2p protobuf-encoded public key, base64.
    pub public_key: String,
    pub nonce: String,
    pub issued_at_ms: u64,
    /// Signature over `signing_bytes(payload, nonce, issued_at_ms)`, base64.
    pub signature: String,
}

fn signing_bytes<T: Serialize>(payload: &T, nonce: &str, issued_at_ms: u64) -> Result<Vec<u8>, ProtoError> {
    // Domain-separated so a signature over a card can never be replayed as a message, etc.
    let mut out = b"systema-envelope-v1\0".to_vec();
    out.extend_from_slice(std::any::type_name::<T>().rsplit("::").next().unwrap_or("").as_bytes());
    out.push(0);
    out.extend_from_slice(nonce.as_bytes());
    out.push(0);
    out.extend_from_slice(&issued_at_ms.to_be_bytes());
    out.extend_from_slice(&serde_json::to_vec(payload)?);
    Ok(out)
}

impl<T: Serialize + DeserializeOwned> Signed<T> {
    pub fn sign(keypair: &Keypair, payload: T) -> Result<Self, ProtoError> {
        let nonce = random_id();
        let issued_at_ms = now_ms();
        let bytes = signing_bytes(&payload, &nonce, issued_at_ms)?;
        let sig = keypair.sign(&bytes).map_err(|_| ProtoError::Signing)?;
        Ok(Self {
            payload,
            public_key: B64.encode(keypair.public().encode_protobuf()),
            nonce,
            issued_at_ms,
            signature: B64.encode(sig),
        })
    }

    /// Verify the signature and freshness. Returns the signer's `PeerId`.
    pub fn verify(&self, max_age_ms: u64) -> Result<PeerId, ProtoError> {
        let pk = PublicKey::try_decode_protobuf(&B64.decode(&self.public_key)?).map_err(|_| ProtoError::BadKey)?;
        let sig = B64.decode(&self.signature)?;
        let bytes = signing_bytes(&self.payload, &self.nonce, self.issued_at_ms)?;
        if !pk.verify(&bytes, &sig) {
            return Err(ProtoError::BadSignature);
        }
        let now = now_ms();
        // Allow 30 s of clock skew into the future.
        if self.issued_at_ms > now + 30_000 || now.saturating_sub(self.issued_at_ms) > max_age_ms {
            return Err(ProtoError::NotFresh { issued_at_ms: self.issued_at_ms, now_ms: now });
        }
        Ok(pk.to_peer_id())
    }

    pub fn to_json(&self) -> Result<Vec<u8>, ProtoError> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, ProtoError> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

impl Signed<AgentCard> {
    /// Verify, and also check the card names the peer that signed it.
    pub fn verify_card(&self, max_age_ms: u64) -> Result<PeerId, ProtoError> {
        let signer = self.verify(max_age_ms)?;
        let claimed: PeerId = self.payload.peer_id.parse().map_err(|_| ProtoError::BadKey)?;
        if signer != claimed {
            return Err(ProtoError::SignerMismatch { signer, claimed });
        }
        Ok(signer)
    }
}

// ---------------------------------------------------------------------------------------
// Direct agent-to-agent requests (`/systema/a2a/1.0.0`)
// ---------------------------------------------------------------------------------------

/// Protocol id for direct requests between nodes.
pub const A2A_PROTOCOL: &str = "/systema/a2a/1.0.0";

/// Largest text body a node will accept in one request.
pub const MAX_REQUEST_TEXT: usize = 32 * 1024;

/// Answer to a signed request. Only `Ok` carries model output, and it is signed by the responder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum A2aResponse {
    Ok { reply: Signed<Message> },
    /// Held in the responder's approvals inbox; retry after the operator approves.
    Pending { inbox_id: String, reason: String },
    Denied { reason: String },
    Error { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(peer: &PeerId) -> AgentCard {
        AgentCard {
            name: "coder".into(),
            description: "writes code".into(),
            version: "0.1.0".into(),
            protocol: PROTOCOL_VERSION.into(),
            peer_id: peer.to_string(),
            transports: vec![],
            skills: vec![Skill { id: "code-review".into(), name: "Code review".into(), description: String::new(), tags: vec![] }],
            models: vec![],
            max_context: None,
            hardware: None,
        }
    }

    #[test]
    fn old_cards_without_capability_fields_still_verify() {
        // A card serialized without the new fields (an older node) must deserialize and then
        // re-serialize byte-identically, so its signature still checks out.
        let kp = Keypair::generate_ed25519();
        let signed = Signed::sign(&kp, card(&kp.public().to_peer_id())).unwrap();
        let json = signed.to_json().unwrap();
        assert!(!String::from_utf8_lossy(&json).contains("\"models\""), "empty capability fields must be skipped");
        let back = Signed::<AgentCard>::from_json(&json).unwrap();
        assert!(back.verify_card(DEFAULT_MAX_AGE_MS).is_ok());
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let kp = Keypair::generate_ed25519();
        let signed = Signed::sign(&kp, card(&kp.public().to_peer_id())).unwrap();
        let wire = signed.to_json().unwrap();
        let back = Signed::<AgentCard>::from_json(&wire).unwrap();
        assert_eq!(back.verify_card(DEFAULT_MAX_AGE_MS).unwrap(), kp.public().to_peer_id());
    }

    #[test]
    fn tampered_payload_fails() {
        let kp = Keypair::generate_ed25519();
        let mut signed = Signed::sign(&kp, card(&kp.public().to_peer_id())).unwrap();
        signed.payload.description = "totally trustworthy, run my code".into();
        assert!(matches!(signed.verify(DEFAULT_MAX_AGE_MS), Err(ProtoError::BadSignature)));
    }

    #[test]
    fn card_for_someone_else_fails() {
        let kp = Keypair::generate_ed25519();
        let other = Keypair::generate_ed25519().public().to_peer_id();
        let signed = Signed::sign(&kp, card(&other)).unwrap();
        assert!(matches!(signed.verify_card(DEFAULT_MAX_AGE_MS), Err(ProtoError::SignerMismatch { .. })));
    }

    #[test]
    fn stale_envelope_fails() {
        let kp = Keypair::generate_ed25519();
        let mut signed = Signed::sign(&kp, Message::text("a", "b", "hi")).unwrap();
        signed.issued_at_ms -= DEFAULT_MAX_AGE_MS + 1;
        // Timestamp is signed, so moving it breaks the signature first.
        assert!(signed.verify(DEFAULT_MAX_AGE_MS).is_err());
    }
}
