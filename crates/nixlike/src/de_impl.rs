use std::convert::{TryFrom, TryInto};

use linked_hash_map::LinkedHashMap;
use serde::{
	Deserializer,
	de::{self, MapAccess, SeqAccess},
};

use crate::{Error, Value};

struct ObjectAccess {
	iter: linked_hash_map::IntoIter<String, Value>,
	value: Option<Value>,
}
impl ObjectAccess {
	fn new(v: LinkedHashMap<String, Value>) -> Self {
		Self {
			iter: v.into_iter(),
			value: None,
		}
	}
}

impl<'de> MapAccess<'de> for ObjectAccess {
	type Error = Error;

	fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>, Self::Error>
	where
		K: de::DeserializeSeed<'de>,
	{
		match self.iter.next() {
			Some((k, v)) => {
				let _ = self.value.insert(v);
				Ok(Some(seed.deserialize(Value::String(k))?))
			}
			_ => Ok(None),
		}
	}

	fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value, Self::Error>
	where
		V: de::DeserializeSeed<'de>,
	{
		seed.deserialize(self.value.take().unwrap())
	}
}

struct ArrayAccess {
	iter: std::vec::IntoIter<Value>,
}
impl ArrayAccess {
	fn new(v: Vec<Value>) -> Self {
		Self {
			iter: v.into_iter(),
		}
	}
}

impl<'de> SeqAccess<'de> for ArrayAccess {
	type Error = Error;

	fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>, Self::Error>
	where
		T: de::DeserializeSeed<'de>,
	{
		match self.iter.next() {
			Some(v) => Ok(Some(seed.deserialize(v)?)),
			_ => Ok(None),
		}
	}
}

impl Value {
	fn parse_int<T: TryFrom<i64>>(&self) -> Result<T, Error> {
		match self {
			Value::Number(n) => Ok((*n).try_into().map_err(|_| Error::BadNumber)?),
			_ => Err(Error::Expected("integer")),
		}
	}
	fn parse_boolean(self) -> Result<bool, Error> {
		match self {
			Value::Boolean(b) => Ok(b),
			_ => Err(Error::Expected("boolean")),
		}
	}
	pub fn parse_string(&self) -> Result<&str, Error> {
		match self {
			Value::String(s) => Ok(s),
			_ => Err(Error::Expected("string")),
		}
	}
	fn parse_char(self) -> Result<char, Error> {
		match self {
			Value::String(s) if s.chars().count() == 1 => Ok(s.chars().next().unwrap()),
			_ => Err(Error::Expected("char")),
		}
	}
	fn parse_array(self) -> Result<Vec<Value>, Error> {
		match self {
			Value::Array(s) => Ok(s),
			_ => Err(Error::Expected("array")),
		}
	}
	fn parse_object(self) -> Result<LinkedHashMap<String, Value>, Error> {
		match self {
			Value::Object(s) => Ok(s),
			_ => Err(Error::Expected("object")),
		}
	}
	fn parse_null(self) -> Result<(), Error> {
		match self {
			Value::Null => Ok(()),
			_ => Err(Error::Expected("null")),
		}
	}
}

impl de::Error for Error {
	fn custom<T>(msg: T) -> Self
	where
		T: std::fmt::Display,
	{
		Self::Custom(format!("{}", msg))
	}
}

impl<'de> Deserializer<'de> for Value {
	type Error = Error;

	fn deserialize_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		match self {
			Value::Number(f) => visitor.visit_i64(f),
			Value::String(s) => visitor.visit_str(&s),
			Value::Boolean(b) => visitor.visit_bool(b),
			Value::Object(o) => visitor.visit_map(ObjectAccess::new(o)),
			Value::Array(a) => visitor.visit_seq(ArrayAccess::new(a)),
			Value::Null => visitor.visit_none(),
		}
	}

	fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_bool(self.parse_boolean()?)
	}

	fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_i8(self.parse_int()?)
	}

	fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_i16(self.parse_int()?)
	}

	fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_i32(self.parse_int()?)
	}

	fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_i64(self.parse_int()?)
	}

	fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_u8(self.parse_int()?)
	}

	fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_u16(self.parse_int()?)
	}

	fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_u32(self.parse_int()?)
	}

	fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_u64(self.parse_int()?)
	}

	fn deserialize_f32<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		todo!()
	}

	fn deserialize_f64<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		todo!()
	}

	fn deserialize_char<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_char(self.parse_char()?)
	}

	fn deserialize_str<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_str(self.parse_string()?)
	}

	fn deserialize_string<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_string(self.parse_string()?.to_owned())
	}

	fn deserialize_bytes<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		todo!()
	}

	fn deserialize_byte_buf<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		todo!()
	}

	fn deserialize_option<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		match self {
			Value::Null => visitor.visit_none(),
			v => visitor.visit_some(v),
		}
	}

	fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		self.parse_null()?;
		visitor.visit_unit()
	}

	fn deserialize_unit_struct<V>(
		self,
		_name: &'static str,
		visitor: V,
	) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		self.deserialize_unit(visitor)
	}

	fn deserialize_newtype_struct<V>(
		self,
		_name: &'static str,
		visitor: V,
	) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_newtype_struct(self)
	}

	fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_seq(self.parse_array().map(ArrayAccess::new)?)
	}

	fn deserialize_tuple<V>(self, _len: usize, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		self.deserialize_seq(visitor)
	}

	fn deserialize_tuple_struct<V>(
		self,
		_name: &'static str,
		_len: usize,
		visitor: V,
	) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		self.deserialize_seq(visitor)
	}

	fn deserialize_map<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		visitor.visit_map(self.parse_object().map(ObjectAccess::new)?)
	}

	fn deserialize_struct<V>(
		self,
		_name: &'static str,
		_fields: &'static [&'static str],
		visitor: V,
	) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		self.deserialize_map(visitor)
	}

	fn deserialize_enum<V>(
		self,
		_name: &'static str,
		_variants: &'static [&'static str],
		_visitor: V,
	) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		todo!()
	}

	fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		self.deserialize_str(visitor)
	}

	fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value, Self::Error>
	where
		V: serde::de::Visitor<'de>,
	{
		self.deserialize_any(visitor)
	}
}
