use std::collections::HashMap;
use std::marker::PhantomData;

use remowt_polkit_shared::{BackendRequest, Identity};
use remowt_ui_prompt::dbus::DbusPrompterInterface;
use remowt_ui_prompt::Prompter;
use zbus::Connection;

use crate::PolkitHelperProxy;

use super::Helper;

struct TemporaryPrompterInterface<P: Prompter + 'static> {
	connection: Connection,
	path: String,
	_marker: PhantomData<P>,
}
impl<P: Prompter + 'static> TemporaryPrompterInterface<P> {
	async fn new(connection: Connection, prompter: P) -> Self {
		let path = format!(
			"/remowt/prompters/{}",
			uuid::Uuid::new_v4().to_string().replace("-", "_")
		);
		let _ = connection
			.object_server()
			.at(path.clone(), DbusPrompterInterface(prompter))
			.await;
		Self {
			connection,
			path,
			_marker: PhantomData,
		}
	}
}
impl<P: Prompter + Send + Sync + 'static> Drop for TemporaryPrompterInterface<P> {
	fn drop(&mut self) {
		// Removal is async because of async RwLock used inside...
		// We should not care about its reuse
		let connection = self.connection.clone();
		let path = std::mem::take(&mut self.path);
		tokio::spawn(async move {
			let _ = connection
				.object_server()
				.remove::<DbusPrompterInterface<P>, String>(path)
				.await;
		});
	}
}

#[derive(Clone)]
pub struct DbusHelper {
	connection: Connection,
	helper: PolkitHelperProxy<'static>,
}
impl DbusHelper {
	pub async fn new(connection: Connection) -> zbus::Result<Self> {
		let helper = PolkitHelperProxy::new(&connection).await?;
		Ok(Self { connection, helper })
	}
}
impl Helper for DbusHelper {
	async fn help_me<P: Prompter + Send + Sync + 'static>(
		&self,
		cookie: &str,
		prompter: P,
		identity: Identity,
	) -> anyhow::Result<()> {
		let prompter = TemporaryPrompterInterface::new(self.connection.clone(), prompter).await;
		self.helper
			.init_conversation(
				BackendRequest {
					cookie: cookie.to_owned(),
					environment: HashMap::new(),
					prompter_path: prompter.path.clone(),
					identity,
				}, // cookie.to_owned(), HashMap::new(), prompter.path.clone()
			)
			.await?;
		Ok(())
	}
}
