use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::Sha256;
use zeroize::{Zeroize, ZeroizeOnDrop};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Pseudonymizer {
    key: Vec<u8>,
}

impl std::fmt::Debug for Pseudonymizer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pseudonymizer").finish_non_exhaustive()
    }
}

impl Pseudonymizer {
    pub fn from_base64(encoded: &str) -> Result<Self> {
        let key = STANDARD
            .decode(encoded.trim())
            .context("pseudonym_key_b64 is not valid base64")?;
        if key.len() < 32 {
            bail!("pseudonym key must contain at least 256 bits");
        }
        Ok(Self { key })
    }

    pub fn generate_base64() -> String {
        let mut key = [0_u8; 32];
        rand::rng().fill_bytes(&mut key);
        let encoded = STANDARD.encode(key);
        key.zeroize();
        encoded
    }

    pub fn id(&self, namespace: &str, value: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(b"scaling-neuro\0");
        mac.update(namespace.as_bytes());
        mac.update(b"\0");
        mac.update(value.trim().as_bytes());
        let digest = mac.finalize().into_bytes();
        hex::encode(&digest[..12])
    }

    pub fn subject_id(&self, patient_id: &str, issuer: Option<&str>) -> String {
        let value = format!(
            "{}\0{}",
            issuer.unwrap_or_default().trim(),
            patient_id.trim()
        );
        self.id("subject", &value)
    }

    pub fn protocol_group_id(&self, protocol: &str) -> String {
        let normalized = normalize_protocol(protocol);
        self.id(
            "protocol-group",
            if normalized.is_empty() {
                protocol
            } else {
                &normalized
            },
        )
    }
}

pub fn normalize_protocol(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut last_was_separator = false;
    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            last_was_separator = false;
        } else if !last_was_separator {
            output.push('_');
            last_was_separator = true;
        }
    }
    output.trim_matches('_').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pseudonymizer() -> Pseudonymizer {
        Pseudonymizer::from_base64(&STANDARD.encode([7_u8; 32])).unwrap()
    }

    #[test]
    fn ids_are_stable_and_domain_separated() {
        let p = pseudonymizer();
        assert_eq!(p.id("study", "1.2.3"), p.id("study", "1.2.3"));
        assert_ne!(p.id("study", "1.2.3"), p.id("series", "1.2.3"));
        assert_eq!(p.id("study", "1.2.3").len(), 24);
    }

    #[test]
    fn protocol_normalization_removes_formatting_variation() {
        assert_eq!(
            normalize_protocol(" Resting-State / AP "),
            "resting_state_ap"
        );
    }

    #[test]
    fn non_ascii_protocols_do_not_collapse_to_empty() {
        let p = pseudonymizer();
        assert_ne!(p.protocol_group_id("휴식"), p.protocol_group_id("과제"));
    }
}
