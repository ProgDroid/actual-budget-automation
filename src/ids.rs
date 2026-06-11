//! Strongly-typed identifiers.
//!
//! Each id wraps a `String` but is a distinct type, so the compiler rejects
//! mixing (e.g. a Monzo account id where an Actual account id is wanted).
//!
//! **Serde is `transparent`**: on the wire (`state.json`, the Node shim JSON
//! protocol) each id is just its inner string — byte-identical to the
//! pre-newtype representation. Validation lives ONLY in the smart constructors
//! (`new`), which are called at the Monzo JSON-parse ingestion boundary.
//! `Deserialize` is deliberately NOT validated, so a persisted state file or a
//! shim response never fails to load. `Default` likewise yields an unvalidated
//! empty id for struct-update ergonomics in tests.
//!
//! Two construction paths, by intent:
//! - `new(..) -> Result<Self, IdError>` — **validating**; use at the Monzo
//!   ingestion boundary where bad data must be rejected. (`ActualAccountId` has
//!   no `new`: see the note on its `impl`.)
//! - `From<String>` / `From<&str>` — **unchecked**; use for in-memory moves and
//!   for reconstructing already-trusted persisted ids (the in-memory twin of the
//!   unchecked transparent `Deserialize`).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Error from a validating id constructor.
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum IdError {
    #[error("expected a Monzo account id starting with `acc_`, got {0:?}")]
    MonzoAccount(String),
    #[error("expected a Monzo pot id starting with `pot_`, got {0:?}")]
    MonzoPot(String),
    #[error("external transaction id must not be empty")]
    EmptyExternalTx,
}

macro_rules! string_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(
            Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
            Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// The id as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        // Unchecked construction (see module docs): for in-memory moves and
        // reconstructing already-trusted persisted ids. Validation lives in
        // `new`, used at ingestion boundaries only.
        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }
    };
}

string_newtype! {
    /// Monzo account id (`acc_…`).
    MonzoAccountId
}
string_newtype! {
    /// Monzo pot id (`pot_…`).
    MonzoPotId
}
string_newtype! {
    /// Actual account UUID — the import / transfer target.
    ActualAccountId
}
string_newtype! {
    /// A bank's stable transaction id; used as Actual's `imported_id`.
    ExternalTxId
}

impl MonzoAccountId {
    /// Construct from a raw string, validating the `acc_` prefix.
    ///
    /// # Errors
    /// Returns [`IdError::MonzoAccount`] if `s` does not start with `acc_`.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.starts_with("acc_") {
            Ok(Self(s))
        } else {
            Err(IdError::MonzoAccount(s))
        }
    }
}

impl MonzoPotId {
    /// Construct from a raw string, validating the `pot_` prefix.
    ///
    /// # Errors
    /// Returns [`IdError::MonzoPot`] if `s` does not start with `pot_`.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.starts_with("pot_") {
            Ok(Self(s))
        } else {
            Err(IdError::MonzoPot(s))
        }
    }
}

// `ActualAccountId` has no validating `new`: its values originate only from
// Actual's own database via the trusted (transparent, unchecked) `Deserialize`
// path, or from the `opt_actual_empty` adapter below — never from an untrusted
// raw string. The empty-string hazard a constructor would guard is structurally
// impossible here: "unmapped" is modelled as `Option::None`, not `""`.

impl ExternalTxId {
    /// Construct from a raw string, rejecting the empty string.
    ///
    /// # Errors
    /// Returns [`IdError::EmptyExternalTx`] if `s` is empty.
    pub fn new(s: impl Into<String>) -> Result<Self, IdError> {
        let s = s.into();
        if s.is_empty() {
            Err(IdError::EmptyExternalTx)
        } else {
            Ok(Self(s))
        }
    }
}

/// Serde adapter for `Option<ActualAccountId>` fields whose on-disk form is a
/// string where `""` means "unmapped". Preserves byte-compatibility with state
/// files written before the `Option` modelling.
pub mod opt_actual_empty {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::ActualAccountId;

    /// # Errors
    /// Propagates the serializer's error.
    // serde's `#[serde(with = ...)]` contract fixes this signature to take
    // `&Option<T>`; `Option<&T>` would not satisfy it.
    #[allow(clippy::ref_option)]
    pub fn serialize<S: Serializer>(v: &Option<ActualAccountId>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(id) => s.serialize_str(id.as_str()),
            None => s.serialize_str(""),
        }
    }

    /// # Errors
    /// Propagates the deserializer's error.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<ActualAccountId>, D::Error> {
        let s = String::deserialize(d)?;
        Ok(if s.is_empty() {
            None
        } else {
            // Bypass validation: trust what was previously persisted.
            Some(ActualAccountId(s))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monzo_account_validates_prefix() {
        assert!(MonzoAccountId::new("acc_123").is_ok());
        assert_eq!(
            MonzoAccountId::new("pot_123"),
            Err(IdError::MonzoAccount("pot_123".to_owned()))
        );
    }

    #[test]
    fn monzo_pot_validates_prefix() {
        assert!(MonzoPotId::new("pot_123").is_ok());
        assert!(MonzoPotId::new("acc_123").is_err());
    }

    #[test]
    fn external_tx_rejects_empty() {
        assert!(ExternalTxId::new("tx_1").is_ok());
        assert_eq!(ExternalTxId::new(""), Err(IdError::EmptyExternalTx));
    }

    #[test]
    fn serde_is_transparent() {
        let id = MonzoAccountId::new("acc_9").expect("valid");
        let json = serde_json::to_string(&id).expect("ser");
        assert_eq!(json, "\"acc_9\"");
        let back: MonzoAccountId = serde_json::from_str(&json).expect("de");
        assert_eq!(back, id);
    }

    #[test]
    fn deserialize_does_not_validate() {
        // A persisted-but-malformed id must still load (no brick).
        let back: MonzoAccountId = serde_json::from_str("\"garbage\"").expect("de");
        assert_eq!(back.as_str(), "garbage");
    }

    #[test]
    fn opt_actual_empty_roundtrips_sentinel() {
        #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
        struct Holder {
            #[serde(with = "opt_actual_empty")]
            actual_id: Option<ActualAccountId>,
        }
        // "" -> None
        let none: Holder = serde_json::from_str(r#"{"actual_id":""}"#).expect("de");
        assert_eq!(none.actual_id, None);
        // None -> ""  (byte-identical to old files)
        assert_eq!(
            serde_json::to_string(&none).expect("ser"),
            r#"{"actual_id":""}"#
        );
        // populated roundtrips
        let some: Holder = serde_json::from_str(r#"{"actual_id":"uuid-1"}"#).expect("de");
        assert_eq!(
            some.actual_id.as_ref().map(ActualAccountId::as_str),
            Some("uuid-1")
        );
    }
}
