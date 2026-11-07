//! SUPERCONDUCTOR-18: what both sides of a pairing compute, and what the server signs.
//!
//! ```text
//! Mac (key_m)                         service (key_s)
//!  hosts.request{name, nonce_m} ───▶  {key_s, sig_s(pair ‖ nonce_m ‖ key_m), pairing_id}
//!  verify sig_s; code(key_m, key_s)            code(key_m, key_s) ──▶ the signed-in browser
//! ```
//!
//! The code is the first 40 bits of SHA-256(`key_m` ‖ `key_s`) in base32, shown as
//! `XXXX-XXXX`. A man in the middle has to substitute a key, so the two screens differ.
//! The format and normalization are lifted from menger `tickets-api/src/pairing.rs`
//! (TETHER-11); the input here is both keys, not one.
//!
//! Every message the server signs starts with a label of its own, so a signature made
//! for one purpose can't stand for another.

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use std::path::Path;

const PAIR: &[u8] = b"plexus-agents/pair/v1\n";
const ATTACH: &[u8] = b"plexus-agents/attach/v1\n";
const CALL: &[u8] = b"plexus-agents/call/v1\n";

/// What the server signs for `hosts.request`: the Mac's nonce, bound to its key.
pub fn pair_message(nonce: &str, host_key_hex: &str) -> Vec<u8> {
    [PAIR, nonce.as_bytes(), b"\n", host_key_hex.as_bytes()].concat()
}

/// What the server signs for `hosts.attach`: the attaching socket's nonce.
pub fn attach_message(nonce: &str) -> Vec<u8> {
    [ATTACH, nonce.as_bytes()].concat()
}

/// What the server signs on every call it sends a host: (call id, method, nonce).
pub fn call_message(call: &str, method: &str, nonce: &str) -> Vec<u8> {
    let parts: [&[u8]; 6] = [
        CALL,
        call.as_bytes(),
        b"\n",
        method.as_bytes(),
        b"\n",
        nonce.as_bytes(),
    ];
    parts.concat()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 64 lowercase hex (or `key:` and it) as the 32 bytes.
pub fn key_bytes(key: &str) -> Option<[u8; 32]> {
    let h = key.strip_prefix("key:").unwrap_or(key);
    if h.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(h.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// This service's identity key, `key_s`: a deploy secret. Only its public half leaves.
pub struct ServerKey(SigningKey);

impl ServerKey {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    /// The seed in `path` (mode 0600, raw or base64). With `generate`, a missing file
    /// is made with a new seed: for development only.
    pub fn load(path: &Path, generate: bool) -> anyhow::Result<Self> {
        if generate && !path.exists() {
            write_new_seed(path)?;
        }
        let seed =
            crate::peer::read_seed(path).map_err(|e| anyhow::anyhow!("--server-key: {e}"))?;
        Ok(Self::from_seed(seed))
    }

    /// `key:<64 hex>`.
    pub fn public(&self) -> String {
        format!("key:{}", hex(self.0.verifying_key().as_bytes()))
    }

    pub fn public_bytes(&self) -> [u8; 32] {
        *self.0.verifying_key().as_bytes()
    }

    /// The signature over `message`, base64.
    pub fn sign(&self, message: &[u8]) -> String {
        let sig = self.0.sign(message);
        base64::engine::general_purpose::STANDARD.encode(sig.to_bytes())
    }
}

/// A new 32-byte seed in `path`, base64, mode 0600, its directory made if missing.
pub fn write_new_seed(path: &Path) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut seed = [0u8; 32];
    seed[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    seed[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let text = base64::engine::general_purpose::STANDARD.encode(seed);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())?;
    Ok(())
}

/// Whether `signature` (base64) is `server_key`'s over `message`.
pub fn verify(server_key: &str, message: &[u8], signature: &str) -> bool {
    let Some(key) = key_bytes(server_key) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key) else {
        return false;
    };
    let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(signature) else {
        return false;
    };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(&sig) else {
        return false;
    };
    key.verify(message, &sig).is_ok()
}

/// How many base32 characters the code has (40 bits).
pub const CODE_CHARS: usize = 8;

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The code both screens show for this pair of keys: `XXXX-XXXX`, the first 40
/// bits of SHA-256(`key_m` ‖ `key_s`) in RFC 4648 base32.
pub fn code(host_key: &[u8; 32], server_key: &[u8; 32]) -> String {
    let digest = Sha256::new()
        .chain_update(host_key)
        .chain_update(server_key)
        .finalize();
    // Lifted from menger `pairing::code_of_key` (TETHER-11): 8 characters × 5 bits
    // = the digest's first 40 bits (5 bytes).
    let bits = digest[..5]
        .iter()
        .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
    let chars: String = (0..CODE_CHARS)
        .map(|i| ALPHABET[((bits >> (35 - 5 * i)) & 31) as usize] as char)
        .collect();
    format!("{}-{}", &chars[..4], &chars[4..])
}

/// A code as a person typed it, as [`code`] writes it: case, spaces and dashes
/// don't matter, and the digits base32 leaves out read as the letters they look
/// like (0 as O, 1 as I, 8 as B). Anything that can't be a code stays as typed,
/// so it matches none.
pub fn normalize(typed: &str) -> String {
    let chars: String = typed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| match c.to_ascii_uppercase() {
            '0' => 'O',
            '1' => 'I',
            '8' => 'B',
            c => c,
        })
        .collect();
    let ok = chars.len() == CODE_CHARS && chars.bytes().all(|b| ALPHABET.contains(&b));
    if !ok {
        return typed.to_string();
    }
    format!("{}-{}", &chars[..4], &chars[4..])
}

/// The columns a QR code may take: a terminal's 80.
pub const QR_COLUMNS: usize = 80;

/// How [`qr_lines`] paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paint {
    /// Black on white, said in ANSI colours: right on any terminal.
    Ansi,
    /// No colours: the light modules drawn in the terminal's ink, for the
    /// usual light-on-dark terminal (a log file reads it inverted).
    Plain,
}

/// `text` as a QR code in Unicode half blocks, lifted from tether `src/pair.rs`
/// (TETHER-12): two rows of modules to a line and one column each, with the
/// four-module quiet zone a camera needs drawn in. Medium error correction, down to
/// Low when that is what fits [`QR_COLUMNS`]; an error when nothing does.
pub fn qr_lines(text: &str, paint: Paint) -> Result<Vec<String>, String> {
    use qrcode::{Color, EcLevel, QrCode};
    const QUIET: usize = 4;
    let code = [EcLevel::M, EcLevel::L]
        .into_iter()
        .filter_map(|ec| QrCode::with_error_correction_level(text, ec).ok())
        .find(|c| c.width() + 2 * QUIET <= QR_COLUMNS)
        .ok_or_else(|| {
            format!(
                "{} characters do not fit a QR code {QR_COLUMNS} columns wide",
                text.len()
            )
        })?;
    let (w, colors) = (code.width(), code.to_colors());
    let side = w + 2 * QUIET;
    let dark = |x: usize, y: usize| -> bool {
        (QUIET..QUIET + w).contains(&x)
            && (QUIET..QUIET + w).contains(&y)
            && colors[(y - QUIET) * w + (x - QUIET)] == Color::Dark
    };
    // What the ink draws: the dark modules on white, the light ones on a dark terminal.
    let ink = |x: usize, y: usize| y < side && (dark(x, y) == (paint == Paint::Ansi));
    Ok((0..side.div_ceil(2))
        .map(|row| {
            let line: String = (0..side)
                .map(|x| match (ink(x, 2 * row), ink(x, 2 * row + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect();
            match paint {
                Paint::Ansi => format!("\x1b[30;107m{line}\x1b[0m"),
                Paint::Plain => line,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_code_is_the_first_40_bits_of_both_keys_in_base32() {
        // menger's fixture, with the base32 it pins: 0x630dcd2966 is MMG4-2KLG.
        let bits = 0x630dcd2966u64;
        let chars: String = (0..CODE_CHARS)
            .map(|i| ALPHABET[((bits >> (35 - 5 * i)) & 31) as usize] as char)
            .collect();
        assert_eq!(chars, "MMG42KLG");
        let (m, s) = ([1u8; 32], [2u8; 32]);
        let c = code(&m, &s);
        assert_eq!(c.len(), CODE_CHARS + 1);
        assert_eq!(&c[4..5], "-");
        assert_eq!(c, code(&m, &s), "the same on both sides");
        assert_ne!(c, code(&s, &m), "the order counts");
    }

    #[test]
    fn a_substituted_server_key_gives_another_code() {
        let host = [7u8; 32];
        let (real, substitute) = (ServerKey::from_seed([1; 32]), ServerKey::from_seed([2; 32]));
        assert_ne!(
            code(&host, &real.public_bytes()),
            code(&host, &substitute.public_bytes())
        );
    }

    #[test]
    fn a_typed_code_is_normalized() {
        assert_eq!(normalize("mmg4 2klg"), "MMG4-2KLG");
        assert_eq!(normalize("MMG4-2KLG"), "MMG4-2KLG");
        assert_eq!(normalize("0I8A-AAAA"), "OIBA-AAAA");
        assert_eq!(normalize("nope"), "nope");
    }

    #[test]
    fn a_signature_stands_for_its_own_message_and_key_only() {
        let key = ServerKey::from_seed([3; 32]);
        let sig = key.sign(&attach_message("n"));
        assert!(verify(&key.public(), &attach_message("n"), &sig));
        assert!(!verify(&key.public(), &attach_message("m"), &sig));
        assert!(!verify(&key.public(), &call_message("n", "", ""), &sig));
        let other = ServerKey::from_seed([4; 32]);
        assert!(!verify(&other.public(), &attach_message("n"), &sig));
        assert!(!verify(&key.public(), &attach_message("n"), "not base64"));
    }
}
