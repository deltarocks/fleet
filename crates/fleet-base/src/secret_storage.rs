use anyhow::{Result, bail, ensure};
use itertools::Itertools;
use std::fs::{File, metadata};
use std::io::{self, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::{env, fs};

use tempfile::{TempPath, tempfile_in};
use toml_edit::{Document, DocumentMut, Formatted, Item, Value};

struct Name(String);

fn encode_name(name: &str) -> Name {
	assert!(
		!name.starts_with(['_', '.']),
		"groups should not start with _ or ."
	);
	assert!(
		!name.chars().any(|c| c == '/'),
		"group name should not contain internal slash"
	);
	Name(name.to_owned())
}

enum RewriteError {
	ConcurrentCreate,
	ConcurrentDelete,
	ConcurrentModify,
	ConcurrentWrite,
	Io(io::Error),
	Persist(tempfile::PersistError),
}

fn safe_rewrite(
	path: &PathBuf,
	old_content: Option<Vec<u8>>,
	new_content: Option<Vec<u8>>,
) -> Result<(), RewriteError> {
	let mut f = match (old_content.is_some(), new_content.is_some()) {
		(false, true) => match File::create_new(path) {
			Ok(v) => v,
			Err(e) if e.kind() == ErrorKind::AlreadyExists => {
				return Err(RewriteError::ConcurrentCreate);
			}
			Err(e) => return Err(RewriteError::Io(e)),
		},
		(true, _) => match File::open(&path) {
			Ok(v) => v,
			Err(_e) => return Err(RewriteError::ConcurrentDelete),
		},
		(false, false) => match metadata(&path) {
			Err(e) if e.kind() == ErrorKind::NotFound => {
				return Ok(());
			}
			Ok(_) => return Err(RewriteError::ConcurrentCreate),
			Err(e) => return Err(RewriteError::Io(e)),
		},
	};
	f.lock().map_err(RewriteError::Io)?;
	let mut check_content = vec![];
	f.read_to_end(&mut check_content)
		.map_err(RewriteError::Io)?;
	match &old_content {
		Some(old) => {
			if old != &check_content {
				return Err(RewriteError::ConcurrentModify);
			}
		}
		None => {
			if !check_content.is_empty() {
				return Err(RewriteError::ConcurrentDelete);
			}
		}
	}
	if let Some(new_content) = new_content {
		if Some(&new_content) == old_content.as_ref() {
			return Ok(());
		}
		let dir = path.parent().expect("file is in directory, thus not root");
		let mut tempfile = tempfile::Builder::new()
			.prefix(".rewrite-")
			.tempfile_in(dir)
			.map_err(RewriteError::Io)?;
		tempfile.write_all(&new_content).map_err(RewriteError::Io)?;
		tempfile.flush().map_err(RewriteError::Io)?;
		tempfile.persist(path).map_err(RewriteError::Persist)?;
	} else {
		fs::remove_file(path).map_err(RewriteError::Io)?;
	}
	let _ = f.unlock();
	Ok(())
}
fn update_string(path: PathBuf, modify: impl Fn(&mut Option<String>) -> Result<()>) -> Result<()> {
	loop {
		let orig = match fs::read_to_string(&path) {
			Ok(v) => Some(v),
			Err(e) if e.kind() == ErrorKind::NotFound => None,
			Err(e) => return Err(e.into()),
		};
		let mut edit = orig.clone();
		modify(&mut edit);

		match safe_rewrite(&path, orig.map(String::into), edit.map(String::into)) {
			Ok(()) => return Ok(()),
			Err(
				RewriteError::ConcurrentCreate
				| RewriteError::ConcurrentModify
				| RewriteError::ConcurrentWrite
				| RewriteError::ConcurrentDelete,
			) => {
				continue;
			}
			Err(RewriteError::Io(io)) => return Err(io.into()),
			Err(RewriteError::Persist(io)) => return Err(io.into()),
		}
	}
}
fn update_toml(path: PathBuf, modify: impl Fn(&mut DocumentMut) -> Result<()>) -> Result<()> {
	update_string(path, |str| {
		let mut doc = match str {
			None => DocumentMut::new(),
			Some(v) => DocumentMut::from_str(v)?,
		};
		modify(&mut doc)?;
		if doc.is_empty() {
			*str = None
		} else {
			*str = Some(doc.to_string())
		}
		Ok(())
	})
}
fn update_lines(path: PathBuf, modify: impl Fn(&mut Vec<String>) -> Result<()>) -> Result<()> {
	update_string(path, |str| {
		let mut list = if let Some(str) = str {
			str.split('\n').map(|s| s.to_owned()).collect_vec()
		} else {
			vec![]
		};
		let had_end_newline = if list.last().map(|v| v.as_str()) == Some("") {
			list.pop();
			true
		} else {
			false
		};
		modify(&mut list)?;
		if list.is_empty() {
			*str = None
		} else {
			if had_end_newline {
				list.push("".to_owned())
			}
			*str = Some(list.join("\n"));
		}
		Ok(())
	})
}
fn update_section(
	data: &mut Vec<String>,
	start: &str,
	end: &str,
	modify: impl Fn(&mut Vec<String>) -> Result<()>,
) -> Result<()> {
	let first = data
		.iter()
		.enumerate()
		.filter(|(_, v)| *v == start)
		.at_most_one()
		.map_err(|_| anyhow::anyhow!("there should be at most one section start"))?
		.map(|(v, _)| v);
	let last = data
		.iter()
		.enumerate()
		.filter(|(_, v)| *v == end)
		.at_most_one()
		.map_err(|_| anyhow::anyhow!("there should be at most one section end"))?
		.map(|(v, _)| v);

	match (first, last) {
		(None, None) => {
			let mut out = Vec::new();
			modify(&mut out)?;
			if out.is_empty() {
				return Ok(());
			}
			data.push(start.to_owned());
			data.extend(out);
			data.push(end.to_owned());
			Ok(())
		}
		(None, Some(_)) | (Some(_), None) => {
			bail!("mismatched section start/end")
		}
		(Some(first), Some(last)) => {
			ensure!(first < last, "section end should come after start");
			let mut out = data[first + 1..last]
				.iter()
				.map(|v| v.to_owned())
				.collect_vec();
			modify(&mut out)?;
			if out.is_empty() {
				data.drain(first..=last);
			} else {
				data.splice(first + 1..last, out);
			}
			Ok(())
		}
	}
}

struct Group {
	path: PathBuf,
}
impl Group {
	fn new(path: PathBuf) -> Self {
		Self { path }
	}
	fn manage(&self, manager: &str) {}
	fn ensure_managing(&self, manager: &str) {
		if !self.has_stored() {
			return;
		}
		let managed = match fs::read_to_string(self.path.join(".managed_by")) {
			Ok(found_manager) => found_manager.lines().any(|line| line == manager),
			Err(e) if e.kind() == ErrorKind::NotFound => true,
			Err(e) => panic!("{e}"),
		};
		assert!(managed);
	}
	fn has_stored(&self) -> bool {
		match fs::metadata(&self.path) {
			Ok(d) => d.is_dir(),
			Err(e) if e.kind() == ErrorKind::NotFound => false,
			Err(e) => panic!("{e}"),
		}
	}
}

struct Root {
	path: PathBuf,
}
impl Root {
	fn new(path: PathBuf) -> Self {
		Self { path }
	}
	fn subgroup(&self, name: &str) -> Group {
		Group::new(self.path.join(name))
	}
}

#[test]
fn test() {
	let mut data = vec![
		"a".to_owned(),
		"b".to_owned(),
		"start".to_owned(),
		"c".to_owned(),
		"d".to_owned(),
		"end".to_owned(),
		"e".to_owned(),
		"f".to_owned(),
	];
	update_section(&mut data, "start", "end", |a| {
		a.push("vv".to_owned());
		Ok(())
	})
	.unwrap();
	dbg!(&data);
	// for v in 0..1000 {
	// 	update_toml(PathBuf::from("./test.toml"), |e| {
	// 		e.as_table_mut()
	// 			.insert("hello", Item::Value(Value::Integer(Formatted::new(v))));
	// 	})
	// 	.expect("update")
	// }
	// v.subgroup(name)
}
