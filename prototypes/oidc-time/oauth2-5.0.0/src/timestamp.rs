//! Integer-second introspection wire compatibility and the published SDK's UTC domain.
use time::OffsetDateTime;

pub(crate) fn ensure_range(value: OffsetDateTime) -> Result<OffsetDateTime, &'static str> {
    if (-8_334_601_228_800..=8_210_266_876_799).contains(&value.unix_timestamp()) {
        Ok(value)
    } else {
        Err("timestamp outside upstream range")
    }
}

pub(crate) mod ts_seconds_option {
    use super::ensure_range;
    use serde::{Deserialize, Deserializer, Serializer};
    use time::OffsetDateTime;

    pub(crate) fn serialize<S>(
        value: &Option<OffsetDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(date) => serializer.serialize_some(
                &ensure_range(*date)
                    .map_err(serde::ser::Error::custom)?
                    .unix_timestamp(),
            ),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<OffsetDateTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Option<i64> preserves null/missing and refuses strings/fractions/u64 overflow.
        Option::<i64>::deserialize(deserializer)?
            .map(|seconds| {
                let date = OffsetDateTime::from_unix_timestamp(seconds)
                    .map_err(serde::de::Error::custom)?;
                ensure_range(date).map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}
