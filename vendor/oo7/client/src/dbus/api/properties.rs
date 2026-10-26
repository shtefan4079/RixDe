use std::collections::HashMap;

use serde::{
    Deserialize,
    ser::{Serialize, SerializeMap},
};
use zbus::zvariant::{self, Type, Value};

use crate::AsAttributes;

const ITEM_PROPERTY_LABEL: &str = "org.freedesktop.Secret.Item.Label";
const ITEM_PROPERTY_ATTRIBUTES: &str = "org.freedesktop.Secret.Item.Attributes";

const COLLECTION_PROPERTY_LABEL: &str = "org.freedesktop.Secret.Collection.Label";

#[derive(Debug, Type)]
#[zvariant(signature = "a{sv}")]
pub struct Properties {
    label: String,
    attributes: Option<HashMap<String, String>>,
}

impl Properties {
    pub fn for_item(label: &str, attributes: &impl AsAttributes) -> Self {
        Self {
            label: label.to_owned(),
            attributes: Some(attributes.as_attributes()),
        }
    }

    pub fn for_collection(label: &str) -> Self {
        Self {
            label: label.to_owned(),
            attributes: None,
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn attributes(&self) -> Option<&HashMap<String, String>> {
        self.attributes.as_ref()
    }
}

impl Serialize for Properties {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if self.attributes.is_none() {
            let mut map = serializer.serialize_map(Some(1))?;
            map.serialize_entry(COLLECTION_PROPERTY_LABEL, &Value::from(&self.label))?;
            map.end()
        } else {
            let mut map = serializer.serialize_map(Some(2))?;
            map.serialize_entry(ITEM_PROPERTY_LABEL, &Value::from(&self.label))?;
            let mut dict = zbus::zvariant::Dict::new(String::SIGNATURE, String::SIGNATURE);

            if let Some(attributes) = &self.attributes {
                for (key, value) in attributes {
                    dict.add(key, value).expect("Key/Value of correct types");
                }
            }

            map.serialize_entry(ITEM_PROPERTY_ATTRIBUTES, &Value::from(dict))?;
            map.end()
        }
    }
}

impl<'de> Deserialize<'de> for Properties {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let mut map: HashMap<&str, Value<'_>> = HashMap::deserialize(deserializer)?;
        if let Some(value) = map.remove(COLLECTION_PROPERTY_LABEL) {
            if map.contains_key(ITEM_PROPERTY_LABEL) || map.contains_key(ITEM_PROPERTY_ATTRIBUTES) {
                return Err(D::Error::custom(
                    "Collection and item properties cannot be mixed",
                ));
            }
            let label = zvariant::Str::try_from(value)
                .map_err(|_| D::Error::custom("Collection label must be a string"))?;
            Ok(Self::for_collection(&label))
        } else {
            let label = map
                .remove(ITEM_PROPERTY_LABEL)
                .ok_or_else(|| D::Error::missing_field(ITEM_PROPERTY_LABEL))?;
            let label = zvariant::Str::try_from(label)
                .map_err(|_| D::Error::custom("Item label must be a string"))?;
            let attributes = map
                .remove(ITEM_PROPERTY_ATTRIBUTES)
                .ok_or_else(|| D::Error::missing_field(ITEM_PROPERTY_ATTRIBUTES))?;
            let attributes = HashMap::<String, String>::try_from(attributes)
                .map_err(|_| D::Error::custom("Item attributes must be a string-to-string map"))?;
            Ok(Self::for_item(&label, &attributes))
        }
    }
}

#[cfg(test)]
mod tests {
    use zbus::zvariant::{Endian, Type, serialized::Context, to_bytes};

    use super::*;

    #[test]
    fn serialize_label() {
        let properties = Properties::for_collection("some_label");

        assert!(properties.attributes().is_none());
        assert_eq!(properties.label(), "some_label");

        let ctxt = Context::new_dbus(Endian::Little, 0);
        let encoded = to_bytes(ctxt, &properties).unwrap();
        let decoded: HashMap<&str, Value<'_>> = encoded.deserialize().unwrap().0;

        assert_eq!(
            decoded[COLLECTION_PROPERTY_LABEL],
            Value::from("some_label")
        );
        assert!(!decoded.contains_key(ITEM_PROPERTY_ATTRIBUTES));
        assert!(!decoded.contains_key(ITEM_PROPERTY_LABEL));
    }

    #[test]
    fn serialize_label_with_attributes() {
        let mut attributes = HashMap::new();
        attributes.insert("some", "attribute");
        let properties = Properties::for_item("some_label", &attributes);

        assert!(properties.attributes().is_some());
        assert_eq!(properties.label(), "some_label");

        let ctxt = Context::new_dbus(Endian::Little, 0);
        let encoded = to_bytes(ctxt, &properties).unwrap();
        let decoded: HashMap<&str, Value<'_>> = encoded.deserialize().unwrap().0;

        assert_eq!(decoded[ITEM_PROPERTY_LABEL], Value::from("some_label"));
        assert!(!decoded.contains_key(COLLECTION_PROPERTY_LABEL));
        assert!(decoded.contains_key(ITEM_PROPERTY_ATTRIBUTES));
        assert_eq!(
            decoded[ITEM_PROPERTY_ATTRIBUTES],
            zvariant::Dict::from(attributes).into()
        );
    }

    #[test]
    fn signature() {
        assert_eq!(Properties::SIGNATURE, "a{sv}");
    }

    #[test]
    fn deserialize_collection_properties() {
        // Create serialized data that represents collection properties
        let mut map = HashMap::new();
        map.insert(COLLECTION_PROPERTY_LABEL, Value::from("test_collection"));

        let ctxt = Context::new_dbus(Endian::Little, 0);
        let encoded = to_bytes(ctxt, &map).unwrap();

        // Deserialize through the Properties Deserialize trait
        let properties: Properties = encoded.deserialize().unwrap().0;

        assert_eq!(properties.label(), "test_collection");
        assert!(properties.attributes().is_none());
    }

    #[test]
    fn deserialize_item_properties() {
        use zbus::zvariant::Dict;

        // Create serialized data that represents item properties
        let mut attrs_dict = Dict::new(String::SIGNATURE, String::SIGNATURE);
        attrs_dict.add("key1", "value1").unwrap();
        attrs_dict.add("key2", "value2").unwrap();

        let mut map = HashMap::new();
        map.insert(ITEM_PROPERTY_LABEL, Value::from("test_item"));
        map.insert(ITEM_PROPERTY_ATTRIBUTES, Value::from(attrs_dict));

        let ctxt = Context::new_dbus(Endian::Little, 0);
        let encoded = to_bytes(ctxt, &map).unwrap();

        // Deserialize through the Properties Deserialize trait
        let properties: Properties = encoded.deserialize().unwrap().0;

        assert_eq!(properties.label(), "test_item");
        let attributes = properties.attributes().unwrap();
        assert_eq!(attributes.get("key1"), Some(&"value1".to_string()));
        assert_eq!(attributes.get("key2"), Some(&"value2".to_string()));
        assert_eq!(attributes.len(), 2);
    }

    #[test]
    fn malformed_properties_return_errors_without_panicking() {
        let valid_attributes = || {
            let mut dict = zbus::zvariant::Dict::new(String::SIGNATURE, String::SIGNATURE);
            dict.add("application", "fixture").unwrap();
            Value::from(dict)
        };
        let cases = [
            HashMap::new(),
            HashMap::from([(ITEM_PROPERTY_LABEL, Value::from("label"))]),
            HashMap::from([(ITEM_PROPERTY_ATTRIBUTES, valid_attributes())]),
            HashMap::from([
                (ITEM_PROPERTY_LABEL, Value::from(17u32)),
                (ITEM_PROPERTY_ATTRIBUTES, valid_attributes()),
            ]),
            HashMap::from([
                (ITEM_PROPERTY_LABEL, Value::from("label")),
                (ITEM_PROPERTY_ATTRIBUTES, Value::from("not a map")),
            ]),
            HashMap::from([(COLLECTION_PROPERTY_LABEL, Value::from(17u32))]),
            HashMap::from([
                (COLLECTION_PROPERTY_LABEL, Value::from("collection")),
                (ITEM_PROPERTY_LABEL, Value::from("item")),
                (ITEM_PROPERTY_ATTRIBUTES, valid_attributes()),
            ]),
        ];
        for (index, map) in cases.into_iter().enumerate() {
            let ctxt = Context::new_dbus(Endian::Little, 0);
            let encoded = to_bytes(ctxt, &map).unwrap();
            assert!(encoded.deserialize::<Properties>().is_err(), "Case {index}");
        }
    }
}
