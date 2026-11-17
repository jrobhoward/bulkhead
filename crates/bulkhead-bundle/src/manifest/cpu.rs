//! CPU written as a number of cores (`0.5`, `2`), as `docker --cpus`
//! takes it: `limits.cpu` (ROADMAP PS6).

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// A share of CPU time in thousandths of a core: 500 is half a core,
/// 2000 two cores.
///
/// Parses from a number of cores with at most three decimals; serializes
/// as that number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Millicores(pub u32);

impl Millicores {
    /// The least a manifest may ask for: 0.01 core, one millisecond in
    /// cgroup's 100 ms period.
    pub const MIN: Self = Self(10);
    /// The most a manifest may ask for: 1024 cores.
    pub const MAX: Self = Self(1_024_000);

    /// Thousandths of a core.
    #[must_use]
    pub const fn millis(self) -> u32 {
        self.0
    }

    /// From a number of cores, if it has at most three decimals and fits.
    #[must_use]
    pub fn from_cores(cores: f64) -> Option<Self> {
        if !cores.is_finite() || cores < 0.0 {
            return None;
        }
        let millis = (cores * 1000.0).round();
        if (cores * 1000.0 - millis).abs() > 1e-6 || millis > f64::from(u32::MAX) {
            return None;
        }
        // In range and whole, checked above.
        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let millis = millis as u32;
        Some(Self(millis))
    }
}

impl fmt::Display for Millicores {
    /// As cores: `0.5`, `2`, `1.25`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (whole, frac) = (self.0 / 1000, self.0 % 1000);
        if frac == 0 {
            return write!(f, "{whole}");
        }
        let frac = format!("{frac:03}");
        write!(f, "{whole}.{}", frac.trim_end_matches('0'))
    }
}

impl Serialize for Millicores {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.0.is_multiple_of(1000) {
            serializer.serialize_u32(self.0 / 1000)
        } else {
            serializer.serialize_f64(f64::from(self.0) / 1000.0)
        }
    }
}

impl<'de> Deserialize<'de> for Millicores {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        const EXPECTING: &str = "a number of cores with at most three decimals, such as 0.5 or 2";

        impl de::Visitor<'_> for Visitor {
            type Value = Millicores;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(EXPECTING)
            }

            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Millicores, E> {
                v.checked_mul(1000)
                    .and_then(|m| u32::try_from(m).ok())
                    .map(Millicores)
                    .ok_or_else(|| E::custom("too many cores"))
            }

            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Millicores, E> {
                u64::try_from(v)
                    .map_err(|_| E::custom("cores cannot be negative"))
                    .and_then(|v| self.visit_u64(v))
            }

            fn visit_f64<E: de::Error>(self, v: f64) -> Result<Millicores, E> {
                Millicores::from_cores(v).ok_or_else(|| E::custom(EXPECTING))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

#[cfg(test)]
#[path = "cpu_tests.rs"]
mod cpu_tests;
