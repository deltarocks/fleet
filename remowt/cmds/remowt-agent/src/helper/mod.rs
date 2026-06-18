use futures::Future;
use remowt_polkit_shared::Identity;
use remowt_ui_prompt::Prompter;

mod protocol;
mod socket;
mod suid;

pub use socket::SocketHelper;
pub use suid::SuidHelper;

pub trait Helper {
	fn help_me<P: Prompter + Send + Sync + 'static>(
		&self,
		cookie: &str,
		prompt: P,
		identity: Identity,
	) -> impl Future<Output = anyhow::Result<()>> + Send;
}
