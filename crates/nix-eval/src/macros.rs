#[macro_export]
macro_rules! nix_expr_inner {
	//(@munch_object FIXME: value should be arbitrary nix_expr_inner input... Time to write proc-macro?
	(@obj($o:ident) $field:ident$(, $($tt:tt)*)?) => {{
		$o.insert(
			stringify!($field),
			$crate::Value::from($field),
		);
		$(nix_expr_inner!(@obj($o) $($tt)*);)?
	}};
	(@obj($o:ident) $field:ident: $v:expr$(, $($tt:tt)*)?) => {{
		$o.insert(
			stringify!($field),
			$crate::Value::from($v),
		);
		$(nix_expr_inner!(@obj($o) $($tt)*);)?
	}};
	(@obj($o:ident)) => {{}};
	(Obj { $($tt:tt)* }) => {{
		use $crate::{nix_expr_inner};
		let mut out = std::collections::hash_map::HashMap::new();
		nix_expr_inner!(@obj(out) $($tt)*);
		Value::new_attrs(out)?
	}};
	(@field($o:ident) . $var:ident $($tt:tt)*) => {{
		$o.index_attr(stringify!($var));
		nix_expr_inner!(@field($o) $($tt)*);
	}};
	(@field($o:ident) [{ $v:expr }] $($tt:tt)*) => {{
		$o.push(Index::attr(&$v));
		nix_expr_inner!(@o($o) $($tt)*);
	}};
	(@field($o:ident) [ $($var:tt)+ ] $($tt:tt)*) => {{
		$o.push(Index::Expr($crate::nix_expr_inner!($($var)+)));
		nix_expr_inner!(@o($o) $($tt)*);
	}};
	(@field($o:ident) ($($var:tt)*) $($tt:tt)*) => {
		$o.push(Index::ExprApply($crate::nix_expr_inner!($($var)+)));
		nix_expr_inner!(@o($o) $($tt)*);
	};
	(@field($o:ident)) => {};
	($field:ident $($tt:tt)*) => {{
		use $crate::{nix_expr_inner};
		// might be used if indexed
		#[allow(unused_mut)]
		let mut out = $field.clone();
		nix_expr_inner!(@field(out) $($tt)*);
		out
	}};
	($v:literal) => {{
		use $crate::macros::NixExprBuilder;
		NixExprBuilder::string($v)
	}};
	({$v:expr}) => {{
		$crate::Value::serialized(&$v)?
	}}
}
#[macro_export]
macro_rules! nix_expr {
	($($tt:tt)+) => {{
		use $crate::{macros::{NixExprBuilder}, Value, nix_expr_inner};
		let expr = nix_expr_inner!($($tt)+);
		Field::new(expr.session(), expr.out)
	}};
}

#[macro_export]
macro_rules! nix_go {
	(@o($o:expr) . $var:ident $($tt:tt)*) => {{
		nix_go!(@o($o.get_field(stringify!($var))?) $($tt)*)
	}};
	(@o($o:expr) [ $v:expr ] $($tt:tt)*) => {{
		nix_go!(@o($o.get_field($v)?) $($tt)*)
	}};
	(@o($o:expr) ($($var:tt)*) $($tt:tt)*) => {
		nix_go!(@o($o.call($crate::nix_expr_inner!($($var)+))?) $($tt)*)
	};
	(@o($o:expr)) => {$o};
	($field:ident $($tt:tt)+) => {{
		use $crate::nix_go;
		let out = $field.clone();
		nix_go!(@o(out) $($tt)*)
	}}
}
#[macro_export]
macro_rules! nix_go_json {
	($($tt:tt)*) => {{
		$crate::nix_go!($($tt)*).as_json()?
	}};
}
