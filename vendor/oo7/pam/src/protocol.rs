//! In-process initial login message; no named-socket serialization exists.
use zeroize::{Zeroize, ZeroizeOnDrop};

#[derive(Zeroize, ZeroizeOnDrop)]
pub struct PamMessage {
    pub username: String,
    pub new_secret: Vec<u8>,
}
impl PamMessage {
    pub fn unlock(username: String, secret: Vec<u8>) -> Self {
        Self {
            username,
            new_secret: secret,
        }
    }
}
