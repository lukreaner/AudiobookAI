use serde::{Serialize, de::DeserializeOwned};

use crate::Result;

pub fn encode<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

pub fn decode<T: DeserializeOwned>(value: &str) -> Result<T> {
    Ok(serde_json::from_str(value)?)
}

pub fn enum_text<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_value(value)?;
    value.as_str().map(ToOwned::to_owned).ok_or_else(|| {
        crate::StorageError::InvalidData("enum did not serialize as a string".into())
    })
}
