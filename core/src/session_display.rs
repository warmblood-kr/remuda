//! Optional, informational session display facts. Never routing or authority.
use crate::Clock;
use core::time::Duration;
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

pub const DISPLAY_TTL: Duration = Duration::from_secs(10);
pub const MAX_SESSION_DISPLAYS: usize = 4096;
pub const MAX_DISPLAY_BYTES: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SessionDisplay {
    pub version: u32,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leader_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_percent: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    pub valid_for_ms: u64,
}

impl SessionDisplay {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.version != 1 || self.provider != "butler" {
            return Err("unsupported display provider or version");
        }
        if self.context_percent.is_some_and(|n| n > 100)
            || self.context_window == Some(0)
            || matches!((self.context_used, self.context_window), (Some(a), Some(b)) if a > b)
        {
            return Err("invalid display context usage");
        }
        // Conservative JSON wire upper bound: field names/punctuation/numbers
        // fit in 384 bytes. Strings count JSON escaping, not just UTF-8 bytes.
        let mut encoded = 384usize;
        for text in [
            Some(&self.provider),
            self.member_id.as_ref(),
            self.leader_id.as_ref(),
            self.alias.as_ref(),
            self.leader.as_ref(),
            self.kind.as_ref(),
            self.model.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if text.len() > 128 || text.chars().any(unsafe_char) {
                return Err("invalid display string");
            }
            encoded += text.len() + text.bytes().filter(|b| matches!(b, b'"' | b'\\')).count();
        }
        if encoded > MAX_DISPLAY_BYTES {
            return Err("display exceeds wire byte cap");
        }
        Ok(())
    }
}
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

// Consume malformed/unknown metadata without rejecting the session listing.
// Values are scalar-only; unknown nested structures are discarded with IgnoredAny.
enum Scalar {
    Text(String),
    Number(u64),
    Invalid,
}
impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct ScalarVisitor;
        impl<'de> Visitor<'de> for ScalarVisitor {
            type Value = Scalar;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a bounded display scalar")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<Scalar, E> {
                Ok(if s.len() <= 128 {
                    Scalar::Text(s.to_owned())
                } else {
                    Scalar::Invalid
                })
            }
            fn visit_string<E: de::Error>(self, s: String) -> Result<Scalar, E> {
                self.visit_str(&s)
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<Scalar, E> {
                Ok(Scalar::Number(n))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<Scalar, E> {
                Ok(u64::try_from(n).map_or(Scalar::Invalid, Scalar::Number))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Scalar, E> {
                Ok(Scalar::Invalid)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Scalar, E> {
                Ok(Scalar::Invalid)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Scalar, E> {
                Ok(Scalar::Invalid)
            }
            fn visit_none<E: de::Error>(self) -> Result<Scalar, E> {
                Ok(Scalar::Invalid)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Scalar, A::Error> {
                while a.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Scalar::Invalid)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Scalar, A::Error> {
                while a.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Scalar::Invalid)
            }
        }
        d.deserialize_any(ScalarVisitor)
    }
}

struct DisplayVisitor;
impl<'de> Visitor<'de> for DisplayVisitor {
    type Value = Option<SessionDisplay>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("optional display metadata")
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        while a.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
        let (fields, invalid) = read_display_fields(&mut a)?;
        Ok(decode_display_fields(fields, invalid))
    }
}

const DISPLAY_FIELDS: &[&str] = &[
    "version",
    "provider",
    "member_id",
    "leader_id",
    "alias",
    "leader",
    "kind",
    "model",
    "context_percent",
    "context_used",
    "context_window",
    "valid_for_ms",
];
fn read_display_fields<'de, A: MapAccess<'de>>(
    a: &mut A,
) -> Result<(HashMap<String, Scalar>, bool), A::Error> {
    let mut fields = HashMap::new();
    let mut invalid = false;
    while let Some(key) = a.next_key::<Scalar>()? {
        let value = a.next_value::<Scalar>()?;
        match key {
            Scalar::Text(key) if DISPLAY_FIELDS.contains(&key.as_str()) => {
                invalid |= fields.insert(key, value).is_some();
            }
            _ => invalid = true,
        }
    }
    Ok((fields, invalid))
}
fn text(fields: &mut HashMap<String, Scalar>, key: &str, invalid: &mut bool) -> Option<String> {
    match fields.remove(key) {
        Some(Scalar::Text(s)) => Some(s),
        None => None,
        _ => {
            *invalid = true;
            None
        }
    }
}
fn number(fields: &mut HashMap<String, Scalar>, key: &str, invalid: &mut bool) -> Option<u64> {
    match fields.remove(key) {
        Some(Scalar::Number(n)) => Some(n),
        None => None,
        _ => {
            *invalid = true;
            None
        }
    }
}

fn decode_display_fields(
    mut fields: HashMap<String, Scalar>,
    mut invalid: bool,
) -> Option<SessionDisplay> {
    let version = number(&mut fields, "version", &mut invalid)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);
    let provider = text(&mut fields, "provider", &mut invalid).unwrap_or_default();
    let member_id = text(&mut fields, "member_id", &mut invalid);
    let leader_id = text(&mut fields, "leader_id", &mut invalid);
    let alias = text(&mut fields, "alias", &mut invalid);
    let leader = text(&mut fields, "leader", &mut invalid);
    let kind = text(&mut fields, "kind", &mut invalid);
    let model = text(&mut fields, "model", &mut invalid);
    let percent = number(&mut fields, "context_percent", &mut invalid);
    let context_percent = percent.and_then(|n| u16::try_from(n).ok());
    if percent.is_some() && context_percent.is_none() {
        invalid = true;
    }
    let context_used = number(&mut fields, "context_used", &mut invalid);
    let context_window = number(&mut fields, "context_window", &mut invalid);
    let valid_for_ms = number(&mut fields, "valid_for_ms", &mut invalid).unwrap_or(0);
    let value = SessionDisplay {
        version,
        provider,
        member_id,
        leader_id,
        alias,
        leader,
        kind,
        model,
        context_percent,
        context_used,
        context_window,
        valid_for_ms,
    };
    (!invalid && valid_for_ms > 0 && valid_for_ms <= 10_000 && value.validate().is_ok())
        .then_some(value)
}

pub fn deserialize_optional_display<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<Option<SessionDisplay>, D::Error> {
    d.deserialize_any(DisplayVisitor)
}
impl<'de> Deserialize<'de> for SessionDisplay {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        deserialize_optional_display(d)?.ok_or_else(|| de::Error::custom("invalid session display"))
    }
}

pub(crate) struct CachedDisplay {
    pub name: String,
    pub value: SessionDisplay,
    pub expires: Duration,
}
#[derive(Default)]
pub(crate) struct DisplayCache {
    pub clock: Option<Arc<dyn Clock>>,
    pub generation: u64,
    pub entries: HashMap<String, CachedDisplay>,
}
impl DisplayCache {
    pub fn replace(
        &mut self,
        generation: u64,
        name: &str,
        instance_id: &str,
        display: Option<SessionDisplay>,
    ) -> Result<(), &'static str> {
        if generation == 0 || self.generation != generation {
            return Err("disposed display generation");
        }
        let now = self
            .clock
            .as_ref()
            .ok_or("display clock unavailable")?
            .now();
        self.prune(now);
        if let Some(mut value) = display {
            if !self.entries.contains_key(instance_id) && self.entries.len() >= MAX_SESSION_DISPLAYS
            {
                return Err("display cache full");
            }
            value.valid_for_ms = DISPLAY_TTL.as_millis() as u64;
            self.entries.insert(
                instance_id.to_owned(),
                CachedDisplay {
                    name: name.to_owned(),
                    value,
                    expires: now.saturating_add(DISPLAY_TTL),
                },
            );
        } else {
            self.entries.remove(instance_id);
        }
        Ok(())
    }
    pub fn prune(&mut self, now: Duration) {
        self.entries.retain(|_, entry| entry.expires > now);
    }
}

#[cfg(test)]
mod decode_tests {
    use super::*;
    use serde::de::{
        value::{Error, MapDeserializer},
        IntoDeserializer,
    };
    enum Value {
        Text(&'static str),
        Number(u64),
        Bool,
    }
    impl<'de> IntoDeserializer<'de, Error> for Value {
        type Deserializer = Self;
        fn into_deserializer(self) -> Self {
            self
        }
    }
    impl<'de> Deserializer<'de> for Value {
        type Error = Error;
        fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
            match self {
                Self::Text(s) => visitor.visit_str(s),
                Self::Number(n) => visitor.visit_u64(n),
                Self::Bool => visitor.visit_bool(false),
            }
        }
        serde::forward_to_deserialize_any! {bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct map struct enum identifier ignored_any}
    }
    fn decode(fields: Vec<(&'static str, Value)>) -> Option<SessionDisplay> {
        deserialize_optional_display(MapDeserializer::<_, Error>::new(fields.into_iter())).unwrap()
    }
    fn fields() -> Vec<(&'static str, Value)> {
        vec![
            ("version", Value::Number(1)),
            ("provider", Value::Text("butler")),
            ("valid_for_ms", Value::Number(10_000)),
            ("alias", Value::Text("worker")),
        ]
    }
    #[test]
    fn valid_metadata_and_malformed_semantic_metadata_do_not_fail_enclosing_decode() {
        assert_eq!(decode(fields()).unwrap().alias.as_deref(), Some("worker"));
        let mut duplicate = fields();
        duplicate.push(("alias", Value::Text("other")));
        assert!(decode(duplicate).is_none());
        for (key, value) in [
            ("version", Value::Number(2)),
            ("provider", Value::Text("other")),
            ("alias", Value::Bool),
            ("valid_for_ms", Value::Number(0)),
            ("valid_for_ms", Value::Number(10_001)),
        ] {
            let mut f = fields();
            f.retain(|(k, _)| *k != key);
            f.push((key, value));
            assert!(decode(f).is_none());
        }
        let mut unknown = fields();
        unknown.push(("secret", Value::Text("never exposed")));
        assert!(decode(unknown).is_none());
        let mut percent = fields();
        percent.push(("context_percent", Value::Number(u64::MAX)));
        assert!(decode(percent).is_none());
        assert!(deserialize_optional_display(Value::Bool).unwrap().is_none());
    }
}
