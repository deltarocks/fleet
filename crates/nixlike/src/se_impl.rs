use std::convert::TryInto;

use linked_hash_map::LinkedHashMap;
use serde::{
	Serializer,
	ser::{
		self, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
		SerializeTupleStruct, SerializeTupleVariant,
	},
};

use crate::{Error, Value};

impl ser::Error for Error {
	fn custom<T>(msg: T) -> Self
	where
		T: std::fmt::Display,
	{
		Self::Custom(format!("{}", msg))
	}
}

pub struct MySerializeSeq(Vec<Value>);

impl SerializeSeq for MySerializeSeq {
	type Ok = Value;

	type Error = Error;

	fn serialize_element<T: ?Sized>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: serde::Serialize,
	{
		self.0.push(value.serialize(MySerialize)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Array(self.0))
	}
}
impl SerializeTuple for MySerializeSeq {
	type Ok = Value;

	type Error = Error;

	fn serialize_element<T: ?Sized>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: serde::Serialize,
	{
		self.0.push(value.serialize(MySerialize)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Array(self.0))
	}
}
impl SerializeTupleStruct for MySerializeSeq {
	type Ok = Value;

	type Error = Error;

	fn serialize_field<T: ?Sized>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: serde::Serialize,
	{
		self.0.push(value.serialize(MySerialize)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Array(self.0))
	}
}

pub struct MySerializeSeqVariant(String, MySerializeSeq);

impl SerializeTupleVariant for MySerializeSeqVariant {
	type Ok = Value;

	type Error = Error;

	fn serialize_field<T: ?Sized>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: serde::Serialize,
	{
		self.1.serialize_field(value)
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Object(
			vec![(self.0, Value::Array(self.1.0))].into_iter().collect(),
		))
	}
}

pub struct MySerializeMap(LinkedHashMap<String, Value>, Option<String>);

impl SerializeMap for MySerializeMap {
	type Ok = Value;

	type Error = Error;

	fn serialize_key<T: ?Sized>(&mut self, key: &T) -> Result<(), Self::Error>
	where
		T: serde::Serialize,
	{
		let _ = self
			.1
			.insert(key.serialize(MySerialize)?.parse_string()?.to_owned());
		Ok(())
	}

	fn serialize_value<T: ?Sized>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: serde::Serialize,
	{
		self.0
			.insert(self.1.take().unwrap(), value.serialize(MySerialize)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Object(self.0))
	}
}

pub struct MySerializeStruct(LinkedHashMap<String, Value>);

impl SerializeStruct for MySerializeStruct {
	type Ok = Value;

	type Error = Error;

	fn serialize_field<T: ?Sized>(&mut self, key: &str, value: &T) -> Result<(), Self::Error>
	where
		T: serde::Serialize,
	{
		self.0.insert(key.to_owned(), value.serialize(MySerialize)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Object(self.0))
	}
}

pub struct MySerializeStructVariant(String, LinkedHashMap<String, Value>);

impl SerializeStructVariant for MySerializeStructVariant {
	type Ok = Value;

	type Error = Error;

	fn serialize_field<T: ?Sized>(
		&mut self,
		key: &'static str,
		value: &T,
	) -> Result<(), Self::Error>
	where
		T: serde::Serialize,
	{
		self.1.insert(key.to_owned(), value.serialize(MySerialize)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Object(
			vec![(self.0, Value::Object(self.1))].into_iter().collect(),
		))
	}
}

pub struct MySerialize;

impl Serializer for MySerialize {
	type Ok = Value;

	type Error = Error;

	type SerializeSeq = MySerializeSeq;

	type SerializeTuple = MySerializeSeq;

	type SerializeTupleStruct = MySerializeSeq;

	type SerializeTupleVariant = MySerializeSeqVariant;

	type SerializeMap = MySerializeMap;

	type SerializeStruct = MySerializeStruct;

	type SerializeStructVariant = MySerializeStructVariant;

	fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Boolean(v))
	}

	fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Number(v as i64))
	}

	fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Number(v as i64))
	}

	fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Number(v as i64))
	}

	fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Number(v))
	}

	fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Number(v as i64))
	}

	fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Number(v as i64))
	}

	fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Number(v as i64))
	}

	fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Number(v.try_into().map_err(|_| Error::BadNumber)?))
	}

	fn serialize_f32(self, _v: f32) -> Result<Self::Ok, Self::Error> {
		todo!()
	}

	fn serialize_f64(self, _v: f64) -> Result<Self::Ok, Self::Error> {
		todo!()
	}

	fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
		Ok(Value::String(v.to_string()))
	}

	fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
		Ok(Value::String(v.to_owned()))
	}

	fn serialize_bytes(self, _v: &[u8]) -> Result<Self::Ok, Self::Error> {
		todo!()
	}

	fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Null)
	}

	fn serialize_some<T: ?Sized>(self, value: &T) -> Result<Self::Ok, Self::Error>
	where
		T: serde::Serialize,
	{
		value.serialize(self)
	}

	fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Null)
	}

	fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
		self.serialize_unit()
	}

	fn serialize_unit_variant(
		self,
		_name: &'static str,
		_variant_index: u32,
		variant: &'static str,
	) -> Result<Self::Ok, Self::Error> {
		Ok(Value::String(variant.to_string()))
	}

	fn serialize_newtype_struct<T: ?Sized>(
		self,
		_name: &'static str,
		value: &T,
	) -> Result<Self::Ok, Self::Error>
	where
		T: serde::Serialize,
	{
		value.serialize(self)
	}

	fn serialize_newtype_variant<T: ?Sized>(
		self,
		_name: &'static str,
		_variant_index: u32,
		variant: &'static str,
		value: &T,
	) -> Result<Self::Ok, Self::Error>
	where
		T: serde::Serialize,
	{
		Ok(Value::Object(
			vec![(variant.to_string(), value.serialize(self)?)]
				.into_iter()
				.collect(),
		))
	}

	fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
		Ok(MySerializeSeq(Vec::with_capacity(len.unwrap_or_default())))
	}

	fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
		Ok(MySerializeSeq(Vec::with_capacity(len)))
	}

	fn serialize_tuple_struct(
		self,
		_name: &'static str,
		len: usize,
	) -> Result<Self::SerializeTupleStruct, Self::Error> {
		Ok(MySerializeSeq(Vec::with_capacity(len)))
	}

	fn serialize_tuple_variant(
		self,
		_name: &'static str,
		_variant_index: u32,
		variant: &'static str,
		len: usize,
	) -> Result<Self::SerializeTupleVariant, Self::Error> {
		Ok(MySerializeSeqVariant(
			variant.to_owned(),
			MySerializeSeq(Vec::with_capacity(len)),
		))
	}

	fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
		Ok(MySerializeMap(LinkedHashMap::new(), None))
	}

	fn serialize_struct(
		self,
		_name: &'static str,
		_len: usize,
	) -> Result<Self::SerializeStruct, Self::Error> {
		Ok(MySerializeStruct(LinkedHashMap::new()))
	}

	fn serialize_struct_variant(
		self,
		_name: &'static str,
		_variant_index: u32,
		variant: &'static str,
		_len: usize,
	) -> Result<Self::SerializeStructVariant, Self::Error> {
		Ok(MySerializeStructVariant(
			variant.to_owned(),
			LinkedHashMap::new(),
		))
	}
}
