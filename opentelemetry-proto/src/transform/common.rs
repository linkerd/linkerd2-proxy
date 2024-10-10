use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) fn to_nanos(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_nanos() as u64
}

use crate::proto::common::v1::{any_value, AnyValue, ArrayValue, InstrumentationScope, KeyValue};
use opentelemetry::{Array, Value};
use std::borrow::Cow;

#[derive(Debug, Default)]
pub struct ResourceAttributesWithSchema {
    pub attributes: Attributes,
    pub schema_url: Option<String>,
}

impl From<&opentelemetry_sdk::Resource> for ResourceAttributesWithSchema {
    fn from(resource: &opentelemetry_sdk::Resource) -> Self {
        ResourceAttributesWithSchema {
            attributes: resource_attributes(resource),
            schema_url: resource.schema_url().map(ToString::to_string),
        }
    }
}

impl
    From<(
        opentelemetry_sdk::InstrumentationLibrary,
        Option<Cow<'static, str>>,
    )> for InstrumentationScope
{
    fn from(
        data: (
            opentelemetry_sdk::InstrumentationLibrary,
            Option<Cow<'static, str>>,
        ),
    ) -> Self {
        let (library, target) = data;
        if let Some(t) = target {
            InstrumentationScope {
                name: t.to_string(),
                version: String::new(),
                attributes: vec![],
                ..Default::default()
            }
        } else {
            InstrumentationScope {
                name: library.name.into_owned(),
                version: library.version.map(Cow::into_owned).unwrap_or_default(),
                attributes: Attributes::from(library.attributes).0,
                ..Default::default()
            }
        }
    }
}

impl
    From<(
        &opentelemetry_sdk::InstrumentationLibrary,
        Option<Cow<'static, str>>,
    )> for InstrumentationScope
{
    fn from(
        data: (
            &opentelemetry_sdk::InstrumentationLibrary,
            Option<Cow<'static, str>>,
        ),
    ) -> Self {
        let (library, target) = data;
        if let Some(t) = target {
            InstrumentationScope {
                name: t.to_string(),
                version: String::new(),
                attributes: vec![],
                ..Default::default()
            }
        } else {
            InstrumentationScope {
                name: library.name.to_string(),
                version: library
                    .version
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                attributes: Attributes::from(library.attributes.clone()).0,
                ..Default::default()
            }
        }
    }
}

/// Wrapper type for Vec<`KeyValue`>
#[derive(Default, Debug)]
pub struct Attributes(pub ::std::vec::Vec<crate::proto::common::v1::KeyValue>);

impl From<Vec<opentelemetry::KeyValue>> for Attributes {
    fn from(kvs: Vec<opentelemetry::KeyValue>) -> Self {
        Attributes(
            kvs.into_iter()
                .map(|api_kv| KeyValue {
                    key: api_kv.key.as_str().to_string(),
                    value: Some(api_kv.value.into()),
                })
                .collect(),
        )
    }
}

impl From<Value> for AnyValue {
    fn from(value: Value) -> Self {
        AnyValue {
            value: match value {
                Value::Bool(val) => Some(any_value::Value::BoolValue(val)),
                Value::I64(val) => Some(any_value::Value::IntValue(val)),
                Value::F64(val) => Some(any_value::Value::DoubleValue(val)),
                Value::String(val) => Some(any_value::Value::StringValue(val.to_string())),
                Value::Array(array) => Some(any_value::Value::ArrayValue(match array {
                    Array::Bool(vals) => array_into_proto(vals),
                    Array::I64(vals) => array_into_proto(vals),
                    Array::F64(vals) => array_into_proto(vals),
                    Array::String(vals) => array_into_proto(vals),
                })),
            },
        }
    }
}

fn array_into_proto<T>(vals: Vec<T>) -> ArrayValue
where
    Value: From<T>,
{
    let values = vals
        .into_iter()
        .map(|val| AnyValue::from(Value::from(val)))
        .collect();

    ArrayValue { values }
}

pub(crate) fn resource_attributes(resource: &opentelemetry_sdk::Resource) -> Attributes {
    resource
        .iter()
        .map(|(k, v)| opentelemetry::KeyValue::new(k.clone(), v.clone()))
        .collect::<Vec<_>>()
        .into()
}
