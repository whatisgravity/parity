// Copyright 2015, 2016 Ethcore (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::Arc;
use ethcore::client::BlockChainClient;
use hypervisor::Hypervisor;
use ethsync::{SyncConfig, NetworkConfiguration, NetworkError};
use ethcore::snapshot::SnapshotService;
#[cfg(not(feature="ipc"))]
use self::no_ipc_deps::*;
#[cfg(feature="ipc")]
use self::ipc_deps::*;
use ethcore_logger::Config as LogConfig;
use std::path::Path;

#[cfg(feature="ipc")]
pub mod service_urls {
	use std::path::PathBuf;

	pub const CLIENT: &'static str = "parity-chain.ipc";
	pub const SNAPSHOT: &'static str = "parity-snapshot.ipc";
	pub const SYNC: &'static str = "parity-sync.ipc";
	pub const SYNC_NOTIFY: &'static str = "parity-sync-notify.ipc";
	pub const NETWORK_MANAGER: &'static str = "parity-manage-net.ipc";

	pub fn with_base(data_dir: &str, service_path: &str) -> String {
		let mut path = PathBuf::from(data_dir);
		path.push(service_path);

		format!("ipc://{}", path.to_str().unwrap())
	}
}

#[cfg(not(feature="ipc"))]
mod no_ipc_deps {
	pub use ethsync::{EthSync, SyncProvider, ManageNetwork};
	pub use ethcore::client::ChainNotify;
}

#[cfg(feature="ipc")]
pub type SyncModules = (
	GuardedSocket<SyncClient<NanoSocket>>,
	GuardedSocket<NetworkManagerClient<NanoSocket>>,
	GuardedSocket<ChainNotifyClient<NanoSocket>>
);

#[cfg(not(feature="ipc"))]
pub type SyncModules = (Arc<SyncProvider>, Arc<ManageNetwork>, Arc<ChainNotify>);

#[cfg(feature="ipc")]
mod ipc_deps {
	pub use ethsync::{SyncClient, NetworkManagerClient, ServiceConfiguration};
	pub use ethcore::client::ChainNotifyClient;
	pub use hypervisor::{SYNC_MODULE_ID, BootArgs, HYPERVISOR_IPC_URL};
	pub use nanoipc::{GuardedSocket, NanoSocket, init_client};
	pub use ipc::IpcSocket;
	pub use ipc::binary::serialize;
}

#[cfg(feature="ipc")]
pub fn hypervisor(base_path: &Path) -> Option<Hypervisor> {
	Some(Hypervisor
		::with_url(&service_urls::with_base(base_path.to_str().unwrap(), HYPERVISOR_IPC_URL))
		.io_path(base_path.to_str().unwrap()))
}

#[cfg(not(feature="ipc"))]
pub fn hypervisor(_: &Path) -> Option<Hypervisor> {
	None
}

#[cfg(feature="ipc")]
fn sync_arguments(io_path: &str, sync_cfg: SyncConfig, net_cfg: NetworkConfiguration, log_settings: &LogConfig) -> BootArgs {
	let service_config = ServiceConfiguration {
		sync: sync_cfg,
		net: net_cfg,
		io_path: io_path.to_owned(),
	};

	// initialisation payload is passed via stdin
	let service_payload = serialize(&service_config).expect("Any binary-derived struct is serializable by definition");

	// client service url and logging settings are passed in command line
	let mut cli_args = Vec::new();
	cli_args.push("sync".to_owned());
	if !log_settings.color { cli_args.push("--no-color".to_owned()); }
	if let Some(ref mode) = log_settings.mode {
		cli_args.push("-l".to_owned());
		cli_args.push(mode.to_owned());
	}
	if let Some(ref file) = log_settings.file {
		cli_args.push("--log-file".to_owned());
		cli_args.push(file.to_owned());
	}

	BootArgs::new().stdin(service_payload).cli(cli_args)
}

#[cfg(feature="ipc")]
pub fn sync
	(
		hypervisor_ref: &mut Option<Hypervisor>,
		sync_cfg: SyncConfig,
		net_cfg: NetworkConfiguration,
		_client: Arc<BlockChainClient>,
		_snapshot_service: Arc<SnapshotService>,
		log_settings: &LogConfig,
	)
	-> Result<SyncModules, NetworkError>
{
	let mut hypervisor = hypervisor_ref.take().expect("There should be hypervisor for ipc configuration");
	let args = sync_arguments(&hypervisor.io_path, sync_cfg, net_cfg, log_settings);
	hypervisor = hypervisor.module(SYNC_MODULE_ID, args);

	hypervisor.start();
	hypervisor.wait_for_startup();

	let sync_client = init_client::<SyncClient<_>>(
		&service_urls::with_base(&hypervisor.io_path, service_urls::SYNC)).unwrap();
	let notify_client = init_client::<ChainNotifyClient<_>>(
		&service_urls::with_base(&hypervisor.io_path, service_urls::SYNC_NOTIFY)).unwrap();
	let manage_client = init_client::<NetworkManagerClient<_>>(
		&service_urls::with_base(&hypervisor.io_path, service_urls::NETWORK_MANAGER)).unwrap();

	*hypervisor_ref = Some(hypervisor);
	Ok((sync_client, manage_client, notify_client))
}

#[cfg(not(feature="ipc"))]
pub fn sync
	(
		_hypervisor: &mut Option<Hypervisor>,
		sync_cfg: SyncConfig,
		net_cfg: NetworkConfiguration,
		client: Arc<BlockChainClient>,
		snapshot_service: Arc<SnapshotService>,
		_log_settings: &LogConfig,
	)
	-> Result<SyncModules, NetworkError>
{
	let eth_sync = try!(EthSync::new(sync_cfg, client, snapshot_service, net_cfg));
	Ok((eth_sync.clone() as Arc<SyncProvider>, eth_sync.clone() as Arc<ManageNetwork>, eth_sync.clone() as Arc<ChainNotify>))
}
