use crate::Value;

pub fn write_identifier(k: &str, out: &mut String) {
	if k.contains(['.', '\'', '\"', '\\', '\n', '\t', '\r', '$']) {
		write_nix_str_singleline(k, out);
	} else {
		out.push_str(k);
	}
}

fn write_nix_obj_key_buf(k: &str, v: &Value, out: &mut String, padding: &mut usize) {
	write_identifier(k, out);
	match v {
		Value::Object(o) if o.len() == 1 => {
			let (k, v) = o.iter().next().unwrap();

			out.push('.');
			write_nix_obj_key_buf(k, v, out, padding);
		}
		v => {
			out.push_str(" = ");
			write_nix_buf(v, out, padding);
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

fn write_padding(out: &mut String, padding: &usize) {
	for _ in 0..*padding {
		out.push_str("  ");
	}
}

pub fn write_nix_str_singleline(str: &str, out: &mut String) {
	out.push_str(&escape_string(str))
}
pub fn write_nix_str(str: &str, out: &mut String, padding: &mut usize) {
	if str.ends_with('\n') {
		out.push_str("''");
		*padding += 1;
		for ele in str[0..str.len() - 1].split('\n') {
			out.push('\n');
			write_padding(out, padding);
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
		out.push('\n');
		*padding -= 1;
		write_padding(out, padding);
		// Final newline is assumed due to str.ends_with condition
		out.push_str("''");
	} else {
		write_nix_str_singleline(str, out);
	}
}

fn write_nix_buf(value: &Value, out: &mut String, padding: &mut usize) {
	match value {
		Value::Null => out.push_str("null"),
		Value::Boolean(v) => out.push_str(if *v { "true" } else { "false" }),
		Value::Number(n) => out.push_str(&format!("{n}")),
		Value::String(s) => write_nix_str(s, out, padding),
		Value::Array(a) => {
			if a.is_empty() {
				out.push_str("[ ]");
			} else {
				out.push_str("[\n");
				*padding += 1;
				for item in a {
					write_padding(out, padding);
					write_nix_buf(item, out, padding);
					out.push('\n');
				}
				*padding -= 1;
				write_padding(out, padding);
				out.push(']');
			}
		}
		Value::Object(obj) => {
			if obj.is_empty() {
				out.push_str("{ }")
			} else {
				out.push_str("{\n");
				*padding += 1;
				for (k, v) in obj {
					write_padding(out, padding);
					write_nix_obj_key_buf(k, v, out, padding);
					out.push('\n');
				}
				*padding -= 1;
				write_padding(out, padding);
				out.push('}');
			}
		}
	};
}

pub fn write_nix(value: &Value) -> String {
	let mut out = String::new();
	write_nix_buf(value, &mut out, &mut 0);
	out
}
