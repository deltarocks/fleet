use std::collections::HashMap;
use std::marker::PhantomData;
use std::mem::forget;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::empty;
use futures_core::stream;
use prodash::messages::MessageLevel;
use prodash::render::tui::{self, ticker};
use prodash::tree::{Item, Root};
use prodash::unit::{Kind, label};
use prodash::{Progress, Unit};
use tokio::join;
use tokio::time::sleep;
use tracing::{Event, Instrument, Level, Span, Subscriber, info, info_span, span, trace};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{self, SubscriberExt as _};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

pub(crate) struct WithContext(fn(&tracing::Dispatch, &span::Id, f: &mut dyn FnMut(&mut Item)));

pub struct ProdashLayer<S> {
	root: Arc<Root>,
	root_logs: Item,
	with_context: WithContext,

	_marker: PhantomData<S>,
}

impl<S> ProdashLayer<S>
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	pub fn new(root: Arc<Root>) -> Self {
		let root_logs = root.add_child("root");
		Self {
			root,
			root_logs,
			with_context: WithContext(Self::get_context),
			_marker: PhantomData,
		}
	}

	fn get_context(dispatch: &tracing::Dispatch, id: &span::Id, f: &mut dyn FnMut(&mut Item)) {
		let subscriber = dispatch
			.downcast_ref::<S>()
			.expect("subscriber should downcast to expected type");
		let span = subscriber.span(id).expect("span should be in context");
		let span = span.as_ref().map(|s| s.extensions());
		let span = span
			.as_ref()
			.map(|e| e.get::<Item>().expect("existence checked"));
	}
}

impl<S> layer::Layer<S> for ProdashLayer<S>
where
	S: Subscriber + for<'a> LookupSpan<'a>,
{
	fn on_event(&self, event: &Event<'_>, ctx: layer::Context<'_, S>) {
		let span = ctx.current_span();
		let cur_span = span
			.id()
			.and_then(|s| ctx.span_scope(s))
			.and_then(|mut scope| {
				scope.find(|span| {
					let ext = span.extensions();
					ext.get::<Item>().is_some()
				})
			});
		let cur_span = cur_span.as_ref().map(|s| s.extensions());
		let cur_span = cur_span
			.as_ref()
			.map(|e| e.get::<Item>().expect("existence checked"));

		let cur_span = cur_span.unwrap_or(&self.root_logs);
		cur_span.info("hello".to_owned());
	}
	fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: layer::Context<'_, S>) {
		let span = ctx.span(id).expect("span should be in context");
		let mut ext = span.extensions_mut();
		ext.insert(self.with_context);

		let mut spans = self.spans.write().expect("not poisoned");
		let name = attrs.metadata().name();
		if let Some(parent) = attrs
			.parent()
			.cloned()
			.or_else(|| {
				if attrs.is_contextual() {
					ctx.current_span().id().cloned()
				} else {
					None
				}
			})
			.and_then(|v| spans.get_mut(&v))
		{
			let child = parent.add_child(name);
			spans.insert(id.clone(), child);
		} else {
			let child = self.root.add_child(name);
			spans.insert(id.clone(), child);
		};
	}
	fn on_close(&self, id: span::Id, _ctx: layer::Context<'_, S>) {
		let mut spans = self.spans.write().expect("not poisoned");
		spans.remove(&id);
	}
}

// #[tokio::test]
// async fn test() {
// 	let root: Arc<Root> = prodash::tree::root::Options {
// 		message_buffer_capacity: 1000,
// 		..Default::default()
// 	}
// 	.create()
// 	.into();
//
// 	let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
// 	let reg = tracing_subscriber::registry()
// 		.with(filter)
// 		.with(ProdashLayer::new(root.clone()));
// 	reg.init();
//
// 	let render = tui::render(
// 		std::io::stdout(),
// 		Arc::downgrade(&root),
// 		tui::Options {
// 			frames_per_second: 10.0,
// 			..tui::Options::default()
// 		},
// 	)
// 	.expect("render");
//
// 	let render = tokio::task::spawn(render);
//
// 	loop {
// 		info!("Hello, world!");
// 		sleep(Duration::from_secs(3))
// 			.instrument(info_span!("sleeping root"))
// 			.await;
// 		async {
// 			sleep(Duration::from_secs(3))
// 				.instrument(info_span!("sleeping 3"))
// 				.await;
// 			sleep(Duration::from_secs(2))
// 				.instrument(info_span!("sleeping 2"))
// 				.await;
// 		}
// 		.instrument(info_span!("sleep parent"))
// 		.await;
// 	}
//
// 	// loop {
// 	// 	sleep(Duration::from_secs(3));
// 	render.await;
// }
