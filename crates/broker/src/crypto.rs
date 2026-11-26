//! Card encryption at rest (ADR 0002, amended 2026-11-21).
//!
//! The broker keeps a small set of cards long-term so a payment can be approved
//! from Discord without re-entering anything. What may be persisted, and in
//! which form, is exactly:
//!
//! * **Clear, PCI-permissible display data** — a user-chosen pet name and the
//!   last four digits. These are what the approval message shows.
//! * **Ciphertext only** — the full card (`card_number`, `expiry`, `cvv`,
//!   `cardholder_name`, `billing_postcode`) serialized and encrypted with
//!   `age` to one deployment recipient. No column anywhere holds a PAN, an
//!   expiry or a CVV, not even a last-four of its own (AGENTS.md rule 6).
//!
//! Plaintext lives in [`CardRecord`], whose fields are [`SecretString`]s and
//! which zeroizes on drop. It is constructed only by the registration CLI
//! (from operator input) or by [`CardCipher::decrypt`] (inside the approved
//! fill path) — and it implements no `Debug`, `Display`, `Clone` or
//! `Serialize`, so it cannot reach a log line, an API response or a panic
//! message by accident (AGENTS.md rule 1, US-14).
//!
//! # Key lifecycle (requirement 2026-11-21)
//!
//! There is no manual keygen and no external key store. The broker generates
//! its own age identity the first time it needs one, writes it into its data
//! volume (`0600`), and reuses it forever after
//! ([`CardCipher::load_or_generate`]). Regenerating on every boot would
//! destroy every stored card, so "generate once, when absent" is the whole
//! contract. Losing the volume means losing the cards — the human keeps the
//! real cards elsewhere (accepted cost, recorded in ADR 0002).
//!
//! The identity file is mounted by deployment, never committed, never written
//! to the database and never logged.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// Environment variable naming the age identity file.
///
/// Deployment sets it (or passes `--identity`); the file lives on the data
/// volume next to the database, and is created on first use.
pub const IDENTITY_ENV_VAR: &str = "TILL_AGE_IDENTITY";

/// The identity file name used when the deployment does not name one.
pub const IDENTITY_FILE_NAME: &str = "age-identity.txt";

/// Longest accepted cardholder name.
const MAX_NAME_LEN: usize = 64;
/// Longest accepted billing postcode.
const MAX_POSTCODE_LEN: usize = 16;
/// Longest accepted expiry string.
const MAX_EXPIRY_LEN: usize = 10;

/// One card's plaintext, wrapped so it zeroizes on drop.
///
/// Fields are [`SecretString`], the type implements no `Debug`, `Display`,
/// `Clone` or `Serialize`, and the only way out is [`CardRecord::fields`] —
/// a borrow that exists for the duration of one Camofox fill call.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct CardRecord {
    card_number: SecretString,
    expiry: SecretString,
    cvv: SecretString,
    cardholder_name: SecretString,
    billing_postcode: SecretString,
}

/// Borrowed view of a card's values, for the fill call and for encryption.
///
/// Deliberately not `Debug`: printing it would print the card.
pub struct CardFields<'a> {
    /// The primary account number, digits only.
    pub card_number: &'a str,
    /// `MM/YY` (or whatever the merchant expects).
    pub expiry: &'a str,
    /// The security code.
    pub cvv: &'a str,
    /// The name on the card.
    pub cardholder_name: &'a str,
    /// Billing postcode / ZIP.
    pub billing_postcode: &'a str,
}

impl CardRecord {
    /// Validate operator input and wrap it.
    ///
    /// The PAN may contain spaces or dashes (they are stripped); everything
    /// else is stored as given, trimmed. Errors name the *field*, never the
    /// value (AGENTS.md rule 1).
    ///
    /// # Errors
    ///
    /// [`CryptoError::MalformedCard`] for any field that fails validation.
    pub fn parse(
        card_number: &str,
        expiry: &str,
        cvv: &str,
        cardholder_name: &str,
        billing_postcode: &str,
    ) -> Result<Self, CryptoError> {
        let card_number = normalize_pan(card_number)?;
        validate_expiry(expiry)?;
        validate_cvv(cvv)?;
        validate_name(cardholder_name)?;
        validate_postcode(billing_postcode)?;

        Ok(Self {
            card_number: SecretString::from(card_number),
            expiry: SecretString::from(expiry.trim()),
            cvv: SecretString::from(cvv.trim()),
            cardholder_name: SecretString::from(cardholder_name.trim()),
            billing_postcode: SecretString::from(billing_postcode.trim()),
        })
    }

    /// Build from a decrypted wire record, moving its buffers into
    /// zeroizing storage rather than copying them.
    fn from_wire(wire: WireCard) -> Result<Self, CryptoError> {
        validate_pan_digits(&wire.card_number)?;
        validate_expiry(&wire.expiry)?;
        validate_cvv(&wire.cvv)?;
        validate_name(&wire.cardholder_name)?;
        validate_postcode(&wire.billing_postcode)?;

        Ok(Self {
            card_number: SecretString::from(wire.card_number),
            expiry: SecretString::from(wire.expiry),
            cvv: SecretString::from(wire.cvv),
            cardholder_name: SecretString::from(wire.cardholder_name),
            billing_postcode: SecretString::from(wire.billing_postcode),
        })
    }

    /// The five values, borrowed.
    ///
    /// The only egress from this type. The fill path builds one, passes it to
    /// the Camofox port, and drops it immediately afterwards.
    #[must_use]
    pub fn fields(&self) -> CardFields<'_> {
        CardFields {
            card_number: self.card_number.expose_secret(),
            expiry: self.expiry.expose_secret(),
            cvv: self.cvv.expose_secret(),
            cardholder_name: self.cardholder_name.expose_secret(),
            billing_postcode: self.billing_postcode.expose_secret(),
        }
    }

    /// The last four digits, the only part of the PAN that may be displayed.
    #[must_use]
    pub fn last4(&self) -> String {
        let pan = self.card_number.expose_secret();
        pan.chars()
            .rev()
            .take(4)
            .collect::<Vec<char>>()
            .into_iter()
            .rev()
            .collect()
    }
}

/// The serialized shape of a card inside the age blob.
///
/// A private helper: the card type itself implements no `Serialize` (ADR 0002),
/// so the wire shape is explicit here and exists only between
/// [`CardCipher::encrypt`] and [`CardCipher::decrypt`].
#[derive(Serialize)]
struct WireCardRef<'a> {
    card_number: &'a str,
    expiry: &'a str,
    cvv: &'a str,
    cardholder_name: &'a str,
    billing_postcode: &'a str,
}

/// The owned form, filled by `serde_json` on decrypt.
#[derive(Deserialize)]
struct WireCard {
    card_number: String,
    expiry: String,
    cvv: String,
    cardholder_name: String,
    billing_postcode: String,
}

/// The deployment's card cipher: one age recipient, one identity.
///
/// Built by [`CardCipher::load_or_generate`] at broker startup (and by the
/// registration CLI, which shares the same volume). Everything it encrypts
/// carries the armored `age` header, so "is this a card blob?" is visible to
/// an operator with `sqlite3` without ever being readable.
pub struct CardCipher {
    identity: age::x25519::Identity,
    recipient: age::x25519::Recipient,
}

impl std::fmt::Debug for CardCipher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The recipient is public key material; the identity never prints.
        formatter
            .debug_struct("CardCipher")
            .field("recipient", &self.recipient.to_string())
            .finish_non_exhaustive()
    }
}

impl CardCipher {
    /// Load the identity at `path`, or generate and persist it if the file is
    /// absent.
    ///
    /// This is the requirement-2026-11-21 lifecycle: generate **once**, when
    /// absent. An existing file is never rewritten — regenerating would make
    /// every stored card undecryptable.
    ///
    /// # Errors
    ///
    /// [`CryptoError::KeyLoad`] if the file exists but cannot be read or
    /// parsed, [`CryptoError::KeyWrite`] if a new identity cannot be
    /// persisted.
    pub fn load_or_generate(path: impl AsRef<Path>) -> Result<Self, CryptoError> {
        let path = path.as_ref();
        match fs::read_to_string(path) {
            Ok(contents) => Self::parse_identity(&contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let cipher = Self::generate();
                match cipher.persist_identity(path) {
                    Ok(()) => Ok(cipher),
                    // Another process won the race: use what it wrote.
                    Err(CryptoError::KeyWrite) if path.exists() => match fs::read_to_string(path) {
                        Ok(contents) => Self::parse_identity(&contents),
                        Err(_) => Err(CryptoError::KeyLoad),
                    },
                    Err(error) => Err(error),
                }
            }
            Err(_) => Err(CryptoError::KeyLoad),
        }
    }

    /// Generate a fresh identity without persisting it (tests, key rotation).
    #[must_use]
    pub fn generate() -> Self {
        let identity = age::x25519::Identity::generate();
        Self::from_identity(identity)
    }

    /// Wrap an existing identity.
    #[must_use]
    pub fn from_identity(identity: age::x25519::Identity) -> Self {
        let recipient = identity.to_public();
        Self {
            identity,
            recipient,
        }
    }

    /// The public recipient, for the operator's records (never the identity).
    #[must_use]
    pub fn recipient_string(&self) -> String {
        self.recipient.to_string()
    }

    /// Encrypt a card to the deployment recipient, ASCII-armored.
    ///
    /// # Errors
    ///
    /// [`CryptoError::Encrypt`] if serialization or encryption fails.
    pub fn encrypt(&self, card: &CardRecord) -> Result<Vec<u8>, CryptoError> {
        let fields = card.fields();
        let wire = WireCardRef {
            card_number: fields.card_number,
            expiry: fields.expiry,
            cvv: fields.cvv,
            cardholder_name: fields.cardholder_name,
            billing_postcode: fields.billing_postcode,
        };
        let mut plaintext = Zeroizing::new(Vec::new());
        serde_json::to_writer(&mut *plaintext, &wire).map_err(|_| CryptoError::Encrypt)?;
        let armored = age::encrypt_and_armor(&self.recipient, &plaintext)
            .map_err(|_| CryptoError::Encrypt)?;
        Ok(armored.into_bytes())
    }

    /// Decrypt one stored blob back into a [`CardRecord`].
    ///
    /// Called from exactly one place — the engine's fill path, for a payment
    /// that reached `approved` — and the record is dropped as soon as the
    /// Camofox fill call returns (AGENTS.md rule 7, ADR 0002).
    ///
    /// # Errors
    ///
    /// [`CryptoError::Decrypt`] for a blob this identity cannot open,
    /// [`CryptoError::CorruptBlob`] for a well-formed blob with bad contents,
    /// [`CryptoError::MalformedCard`] if a field fails validation.
    pub fn decrypt(&self, blob: &[u8]) -> Result<CardRecord, CryptoError> {
        let plaintext =
            Zeroizing::new(age::decrypt(&self.identity, blob).map_err(|_| CryptoError::Decrypt)?);
        let wire: WireCard =
            serde_json::from_slice(&plaintext).map_err(|_| CryptoError::CorruptBlob)?;
        CardRecord::from_wire(wire)
    }

    fn parse_identity(contents: &str) -> Result<Self, CryptoError> {
        let identity: age::x25519::Identity =
            contents.trim().parse().map_err(|_| CryptoError::KeyLoad)?;
        Ok(Self::from_identity(identity))
    }

    fn persist_identity(&self, path: &Path) -> Result<(), CryptoError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|_| CryptoError::KeyWrite)?;
        }

        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // The identity is the only key to every stored card: owner-only.
            options.mode(0o600);
        }

        let Ok(mut file) = options.open(path) else {
            return Err(CryptoError::KeyWrite);
        };
        let serialized = self.identity.to_string();
        let written = file
            .write_all(serialized.expose_secret().as_bytes())
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.sync_all());
        written.map_err(|_| CryptoError::KeyWrite)
    }
}

/// Anything card crypto can refuse. Messages are static text plus a field
/// *kind* — no value can reach them (AGENTS.md rule 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CryptoError {
    /// The identity file exists but could not be read or parsed.
    #[error("the card key file could not be read")]
    KeyLoad,
    /// A freshly generated identity could not be persisted.
    #[error("the card key file could not be written")]
    KeyWrite,
    /// Encryption failed.
    #[error("card encryption failed")]
    Encrypt,
    /// Decryption failed (wrong identity, or a tampered blob).
    #[error("card decryption failed")]
    Decrypt,
    /// The blob decrypted to something that is not a card record.
    #[error("stored card blob is corrupt")]
    CorruptBlob,
    /// A card field failed validation; the string names the field only.
    #[error("invalid card field: {0}")]
    MalformedCard(&'static str),
}

/// Strip spaces and dashes from a PAN and check it looks like one.
fn normalize_pan(raw: &str) -> Result<String, CryptoError> {
    let digits: String = raw
        .chars()
        .filter(|character| !matches!(character, ' ' | '-'))
        .collect();
    validate_pan_digits(&digits)?;
    Ok(digits)
}

fn validate_pan_digits(pan: &str) -> Result<(), CryptoError> {
    if !(12..=19).contains(&pan.len()) || !pan.chars().all(|c| c.is_ascii_digit()) {
        return Err(CryptoError::MalformedCard("card_number"));
    }
    Ok(())
}

fn validate_expiry(expiry: &str) -> Result<(), CryptoError> {
    let trimmed = expiry.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_EXPIRY_LEN {
        return Err(CryptoError::MalformedCard("expiry"));
    }
    Ok(())
}

fn validate_cvv(cvv: &str) -> Result<(), CryptoError> {
    let trimmed = cvv.trim();
    if !(3..=4).contains(&trimmed.len()) || !trimmed.chars().all(|c| c.is_ascii_digit()) {
        return Err(CryptoError::MalformedCard("cvv"));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), CryptoError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_NAME_LEN {
        return Err(CryptoError::MalformedCard("cardholder_name"));
    }
    Ok(())
}

fn validate_postcode(postcode: &str) -> Result<(), CryptoError> {
    let trimmed = postcode.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_POSTCODE_LEN {
        return Err(CryptoError::MalformedCard("billing_postcode"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CardCipher, CardRecord, CryptoError};

    /// Test card values are built at runtime: no literal digit run ever
    /// appears in source, which is exactly what `script/no-card-digits`
    /// enforces (US-14).
    fn test_card() -> CardRecord {
        match CardRecord::parse(
            &"4".repeat(16),
            "12/34",
            &"1".repeat(3),
            "Test Holder",
            &"1".repeat(5),
        ) {
            Ok(card) => card,
            Err(_) => panic!("the synthetic card must be valid"),
        }
    }

    /// `CardRecord` implements no `Debug`, so a failed parse has to be
    /// matched rather than `unwrap_err`ed — which is the point: the type
    /// cannot be printed into a failure message, a log or a trace.
    fn parse_error(
        card_number: &str,
        expiry: &str,
        cvv: &str,
        name: &str,
        postcode: &str,
    ) -> CryptoError {
        match CardRecord::parse(card_number, expiry, cvv, name, postcode) {
            Ok(_) => panic!("expected the card to be rejected"),
            Err(error) => error,
        }
    }

    fn decrypt_error(cipher: &CardCipher, blob: &[u8]) -> CryptoError {
        match cipher.decrypt(blob) {
            Ok(_) => panic!("expected decryption to be refused"),
            Err(error) => error,
        }
    }

    fn plaintext_substrings(card: &CardRecord) -> Vec<String> {
        let fields = card.fields();
        vec![
            fields.card_number.to_string(),
            fields.expiry.to_string(),
            fields.cvv.to_string(),
            fields.cardholder_name.to_string(),
            fields.billing_postcode.to_string(),
        ]
    }

    #[test]
    fn round_trip_preserves_fields_and_ciphertext_is_armored() -> Result<(), CryptoError> {
        let cipher = CardCipher::generate();
        let card = test_card();
        let blob = cipher.encrypt(&card)?;

        let armored = String::from_utf8(blob.clone()).map_err(|_| CryptoError::CorruptBlob)?;
        assert!(
            armored.starts_with("-----BEGIN AGE ENCRYPTED FILE-----"),
            "the blob must be visibly an age file"
        );
        for value in plaintext_substrings(&card) {
            assert!(
                !armored.contains(&value),
                "the ciphertext must not contain a plaintext field"
            );
        }

        let decrypted = cipher.decrypt(&blob)?;
        let before = card.fields();
        let after = decrypted.fields();
        assert_eq!(before.card_number, after.card_number);
        assert_eq!(before.expiry, after.expiry);
        assert_eq!(before.cvv, after.cvv);
        assert_eq!(before.cardholder_name, after.cardholder_name);
        assert_eq!(before.billing_postcode, after.billing_postcode);
        Ok(())
    }

    #[test]
    fn tampered_ciphertext_is_rejected() -> Result<(), CryptoError> {
        let cipher = CardCipher::generate();
        let mut blob = cipher.encrypt(&test_card())?;
        let middle = blob.len() / 2;
        blob[middle] = blob[middle].wrapping_add(1);

        let error = decrypt_error(&cipher, &blob);
        let message = error.to_string();
        for value in plaintext_substrings(&test_card()) {
            assert!(
                !message.contains(&value),
                "a crypto error must never carry card data"
            );
        }
        Ok(())
    }

    #[test]
    fn foreign_identity_cannot_decrypt() -> Result<(), CryptoError> {
        let owner = CardCipher::generate();
        let stranger = CardCipher::generate();
        let blob = owner.encrypt(&test_card())?;

        assert_eq!(decrypt_error(&stranger, &blob), CryptoError::Decrypt);
        Ok(())
    }

    #[test]
    fn parse_validates_shape_normalizes_pan_and_derives_last4() -> Result<(), CryptoError> {
        let pan = "4".repeat(16);
        let spaced = format!("{} {} {}", &pan[..4], &pan[4..8], &pan[8..]);
        let card = CardRecord::parse(&spaced, "12/34", "123", "Holder", "12345")?;
        assert_eq!(card.fields().card_number, pan, "spaces are stripped");
        assert_eq!(card.last4(), pan[12..].to_string());
        assert_eq!(card.last4().chars().count(), 4);

        let too_short = "4".repeat(11);
        assert_eq!(
            parse_error(&too_short, "12/34", "123", "Holder", "12345"),
            CryptoError::MalformedCard("card_number")
        );
        assert_eq!(
            parse_error(&pan, "12/34", "12", "Holder", "12345"),
            CryptoError::MalformedCard("cvv")
        );
        assert_eq!(
            parse_error(&pan, "", "123", "Holder", "12345"),
            CryptoError::MalformedCard("expiry")
        );
        Ok(())
    }

    #[test]
    fn load_or_generate_creates_the_key_once_and_reuses_it() -> Result<(), CryptoError> {
        let dir = std::env::temp_dir().join(format!(
            "till-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let path = dir.join(super::IDENTITY_FILE_NAME);

        let first = CardCipher::load_or_generate(&path)?;
        assert!(path.exists(), "the identity is persisted on first use");
        let recipient = first.recipient_string();

        let contents = std::fs::read_to_string(&path).map_err(|_| CryptoError::KeyLoad)?;
        let second = CardCipher::load_or_generate(&path)?;
        assert_eq!(
            second.recipient_string(),
            recipient,
            "an existing identity is never regenerated"
        );
        assert_eq!(
            std::fs::read_to_string(&path).map_err(|_| CryptoError::KeyLoad)?,
            contents,
            "the key file is not rewritten on every boot"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
