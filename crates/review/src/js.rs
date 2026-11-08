//! JavaScript semantics the ported autoreview code depends on that `jsrt` does not hold: numbers that serialize like
//! JSON.stringify writes them.

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
