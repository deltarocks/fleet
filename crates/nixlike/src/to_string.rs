use alejandra::config::Indentation;

use crate::Value;

pub fn write_identifier(k: &str, out: &mut String) {
	if k.contains(['.', '\'', '\"', '\\', '\n', '\t', '\r', '$']) {
		write_nix_str(k, out);
	} else {
		out.push_str(k);
	}
}

fn write_nix_obj_key_buf(k: &str, v: &Value, out: &mut String) {
	write_identifier(k, out);
	match v {
		Value::Object(o) if o.len() == 1 => {
			let (k, v) = o.iter().next().unwrap();

			out.push('.');
			write_nix_obj_key_buf(k, v, out);
		}
		v => {
			out.push_str(" = ");
			write_nix_buf(v, out);
			out.push(';');
		}
	}
}

pub fn escape_string(str: &str) -> String {
	format!(
		"\"{}\"",
		str.replace('\\', "\\\\")
			.replace('"', "\\\"")
			.replace('\n', "\\n")
			.replace('\t', "\\t")
			.replace('\r', "\\r")
			.replace('$', "\\$")
	)
}

pub fn write_nix_str(str: &str, out: &mut String) {
	if str.ends_with('\n') {
		out.push_str("''");
		for ele in str.split('\n') {
			out.push('\n');
			out.push_str(
				&ele
					// '' is escaped with '
					.replace("''", "'''")
					// ${ is escaped wth ''
					.replace("${", "''${")
					// \t is not counted as whitespace for dedent
					// to avoid confusion, it is printed literally.
					//
					// ...Escaped \t literal should be prefixed with '' for... Idk, this logic is complicated.
					.replace('\t', "''\\t"),
			);
		}
		// Final newline is assumed due to str.ends_with condition
		out.push_str("''");
	} else {
		out.push_str(&escape_string(str))
	}
}

fn write_nix_buf(value: &Value, out: &mut String) {
	match value {
		Value::Null => out.push_str("null"),
		Value::Boolean(v) => out.push_str(if *v { "true" } else { "false" }),
		Value::Number(n) => out.push_str(&format!("{}", n)),
		Value::String(s) => write_nix_str(s, out),
		Value::Array(a) => {
			if a.is_empty() {
				out.push_str("[ ]");
			} else {
				out.push('[');
				for item in a {
					write_nix_buf(item, out);
					out.push('\n');
				}
				out.push(']');
			}
		}
		Value::Object(obj) => {
			if obj.is_empty() {
				out.push_str("{ }")
			} else {
				out.push('{');
				for (k, v) in obj {
					write_nix_obj_key_buf(k, v, out);
					out.push('\n');
				}
				out.push('}');
			}
		}
	};
}

pub fn write_nix(value: &Value) -> String {
	let mut out = String::new();
	write_nix_buf(value, &mut out);
	let (_, out) = alejandra::format::in_memory(
		"".to_owned(),
		out,
		alejandra::config::Config {
			indentation: Indentation::TwoSpaces,
		},
	);
	out
}
