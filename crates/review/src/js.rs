//! JavaScript semantics the ported autoreview code depends on that `jsrt` does not hold: numbers that serialize like
//! JSON.stringify writes them, and `Math.min(max, Math.max(min, value))`.

use serde::{Deserialize, Serialize, Serializer};

/// A JavaScript number: written by JSON.stringify as an integer when it is one.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Deserialize)]
#[serde(transparent)]
pub struct Num(pub f64);

impl Serialize for Num {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        jsrt::json::number(self.0).serialize(serializer)
    }
}

/// `Math.min(max, Math.max(min, value))`.
pub fn clamp(value: f64, min: f64, max: f64) -> f64 {
    max.min(min.max(value))
}
