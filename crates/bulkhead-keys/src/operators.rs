//! The operators file: who may administer a node from another machine.
//!
//! ```toml
//! [operators]
//! alice = ["ed25519:MCowBQYDK2VwAyEA..."]
//! ```

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::key::PublicKey;

/// Each operator's name and accepted keys, from an operators file's text.
///
/// # Errors
///
/// A message naming what is wrong.
pub fn parse_operators(text: &str) -> Result<BTreeMap<String, Vec<PublicKey>>, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OperatorsFile {
        #[serde(default)]
        operators: BTreeMap<String, Vec<String>>,
    }
    let file: OperatorsFile = toml::from_str(text).map_err(|e| e.to_string())?;
    file.operators
        .into_iter()
        .map(|(name, keys)| {
            let keys = keys
                .iter()
                .map(|k| k.parse().map_err(|e| format!("operators.{name}: {e}")))
                .collect::<Result<Vec<PublicKey>, String>>()?;
            Ok((name, keys))
        })
        .collect()
}

/// The name of the operator whose key `key` is.
#[must_use]
pub fn operator_of(
    operators: &BTreeMap<String, Vec<PublicKey>>,
    key: &PublicKey,
) -> Option<String> {
    operators
        .iter()
        .find(|(_, keys)| keys.contains(key))
        .map(|(name, _)| name.clone())
}

#[cfg(test)]
#[path = "operators_tests.rs"]
mod tests;
