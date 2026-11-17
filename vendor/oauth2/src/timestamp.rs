//! Integer-second introspection wire compatibility and the published SDK's UTC domain.
use gateway_datetime_compat::UtcInstant;

pub(crate) fn ensure_range(value: UtcInstant) -> Result<UtcInstant, &'static str> {
    if (-8_334_601_228_800..=8_210_266_876_799).contains(&value.unix_seconds()) {
        Ok(value)
    } else {
        Err("timestamp outside upstream range")
    }
}

pub(crate) mod ts_seconds_option {
    use super::ensure_range;
    use gateway_datetime_compat::UtcInstant;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S>(value: &Option<UtcInstant>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(date) => serializer.serialize_some(
                &ensure_range(*date)
                    .map_err(serde::ser::Error::custom)?
                    .unix_seconds(),
            ),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<UtcInstant>, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Option<i64> preserves null/missing and refuses strings/fractions/u64 overflow.
        Option::<i64>::deserialize(deserializer)?
            .map(|seconds| {
                let date =
                    UtcInstant::from_unix_parts(seconds, 0).map_err(serde::de::Error::custom)?;
                ensure_range(date).map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}
