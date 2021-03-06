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

//! Snapshot network service implementation.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{ManifestData, StateRebuilder, BlockRebuilder};
use super::io::{SnapshotReader, LooseReader, SnapshotWriter, LooseWriter};

use blockchain::BlockChain;
use engines::Engine;
use error::Error;
use service::ClientIoMessage;
use spec::Spec;

use io::IoChannel;

use util::{Bytes, H256, Mutex, RwLock, UtilError};
use util::journaldb::Algorithm;
use util::kvdb::{Database, DatabaseConfig};
use util::snappy;

/// Statuses for restorations.
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum RestorationStatus {
	///	No restoration.
	Inactive,
	/// Ongoing restoration.
	Ongoing,
	/// Failed restoration.
	Failed,
}

/// The interface for a snapshot network service.
/// This handles:
///    - restoration of snapshots to temporary databases.
///    - responding to queries for snapshot manifests and chunks
pub trait SnapshotService {
	/// Query the most recent manifest data.
	fn manifest(&self) -> Option<ManifestData>;

	/// Get raw chunk for a given hash.
	fn chunk(&self, hash: H256) -> Option<Bytes>;

	/// Ask the snapshot service for the restoration status.
	fn status(&self) -> RestorationStatus;

	/// Ask the snapshot service for the number of chunks completed.
	/// Return a tuple of (state_chunks, block_chunks).
	/// Undefined when not restoring.
	fn chunks_done(&self) -> (usize, usize);

	/// Begin snapshot restoration.
	/// If restoration in-progress, this will reset it.
	/// From this point on, any previous snapshot may become unavailable.
	fn begin_restore(&self, manifest: ManifestData);

	/// Abort an in-progress restoration if there is one.
	fn abort_restore(&self);

	/// Feed a raw state chunk to the service to be processed asynchronously.
	/// no-op if not currently restoring.
	fn restore_state_chunk(&self, hash: H256, chunk: Bytes);

	/// Feed a raw block chunk to the service to be processed asynchronously.
	/// no-op if currently restoring.
	fn restore_block_chunk(&self, hash: H256, chunk: Bytes);
}

/// State restoration manager.
struct Restoration {
	manifest: ManifestData,
	state_chunks_left: HashSet<H256>,
	block_chunks_left: HashSet<H256>,
	state: StateRebuilder,
	blocks: BlockRebuilder,
	writer: LooseWriter,
	snappy_buffer: Bytes,
	final_state_root: H256,
}

struct RestorationParams<'a> {
	manifest: ManifestData, // manifest to base restoration on.
	pruning: Algorithm, // pruning algorithm for the database.
	db_path: PathBuf, // database path
	writer: LooseWriter, // writer for recovered snapshot.
	genesis: &'a [u8], // genesis block of the chain.
}

impl Restoration {
	// make a new restoration using the given parameters.
	fn new(params: RestorationParams) -> Result<Self, Error> {
		let manifest = params.manifest;

		let state_chunks = manifest.state_hashes.iter().cloned().collect();
		let block_chunks = manifest.block_hashes.iter().cloned().collect();

		let cfg = DatabaseConfig::with_columns(::db::NUM_COLUMNS);
		let raw_db = Arc::new(try!(Database::open(&cfg, &*params.db_path.to_string_lossy())
			.map_err(UtilError::SimpleString)));

		let chain = BlockChain::new(Default::default(), params.genesis, raw_db.clone());
		let blocks = try!(BlockRebuilder::new(chain, manifest.block_number));

		let root = manifest.state_root.clone();
		Ok(Restoration {
			manifest: manifest,
			state_chunks_left: state_chunks,
			block_chunks_left: block_chunks,
			state: StateRebuilder::new(raw_db, params.pruning),
			blocks: blocks,
			writer: params.writer,
			snappy_buffer: Vec::new(),
			final_state_root: root,
		})
	}

	// feeds a state chunk
	fn feed_state(&mut self, hash: H256, chunk: &[u8]) -> Result<(), Error> {
		if self.state_chunks_left.remove(&hash) {
			let len = try!(snappy::decompress_into(chunk, &mut self.snappy_buffer));

			try!(self.state.feed(&self.snappy_buffer[..len]));
			try!(self.writer.write_state_chunk(hash, chunk));
		}

		Ok(())
	}

	// feeds a block chunk
	fn feed_blocks(&mut self, hash: H256, chunk: &[u8], engine: &Engine) -> Result<(), Error> {
		if self.block_chunks_left.remove(&hash) {
			let len = try!(snappy::decompress_into(chunk, &mut self.snappy_buffer));

			try!(self.blocks.feed(&self.snappy_buffer[..len], engine));
			try!(self.writer.write_block_chunk(hash, chunk));
		}

		Ok(())
	}

	// finish up restoration.
	fn finalize(self) -> Result<(), Error> {
		use util::trie::TrieError;

		if !self.is_done() { return Ok(()) }

		// verify final state root.
		let root = self.state.state_root();
		if root != self.final_state_root {
			warn!("Final restored state has wrong state root: expected {:?}, got {:?}", root, self.final_state_root);
			return Err(TrieError::InvalidStateRoot(root).into());
		}

		// check for missing code.
		try!(self.state.check_missing());

		// connect out-of-order chunks.
		self.blocks.glue_chunks();

		try!(self.writer.finish(self.manifest));

		Ok(())
	}

	// is everything done?
	fn is_done(&self) -> bool {
		self.block_chunks_left.is_empty() && self.state_chunks_left.is_empty()
	}
}

/// Type alias for client io channel.
pub type Channel = IoChannel<ClientIoMessage>;

/// Service implementation.
///
/// This will replace the client's state DB as soon as the last state chunk
/// is fed, and will replace the client's blocks DB when the last block chunk
/// is fed.
pub struct Service {
	restoration: Mutex<Option<Restoration>>,
	client_db: PathBuf, // "<chain hash>/<pruning>/db"
	db_path: PathBuf,  // "<chain hash>/"
	io_channel: Channel,
	pruning: Algorithm,
	status: Mutex<RestorationStatus>,
	reader: RwLock<Option<LooseReader>>,
	engine: Arc<Engine>,
	genesis_block: Bytes,
	state_chunks: AtomicUsize,
	block_chunks: AtomicUsize,
}

impl Service {
	/// Create a new snapshot service.
	pub fn new(spec: &Spec, pruning: Algorithm, client_db: PathBuf, io_channel: Channel) -> Result<Self, Error> {
		let db_path = try!(client_db.parent().and_then(Path::parent)
			.ok_or_else(|| UtilError::SimpleString("Failed to find database root.".into()))).to_owned();

		let reader = {
			let mut snapshot_path = db_path.clone();
			snapshot_path.push("snapshot");
			snapshot_path.push("current");

			LooseReader::new(snapshot_path).ok()
		};

		let service = Service {
			restoration: Mutex::new(None),
			client_db: client_db,
			db_path: db_path,
			io_channel: io_channel,
			pruning: pruning,
			status: Mutex::new(RestorationStatus::Inactive),
			reader: RwLock::new(reader),
			engine: spec.engine.clone(),
			genesis_block: spec.genesis_block(),
			state_chunks: AtomicUsize::new(0),
			block_chunks: AtomicUsize::new(0),
		};

		// create the root snapshot dir if it doesn't exist.
		if let Err(e) = fs::create_dir_all(service.root_dir()) {
			if e.kind() != ErrorKind::AlreadyExists {
				return Err(e.into())
			}
		}

		// delete the temporary restoration dir if it does exist.
		if let Err(e) = fs::remove_dir_all(service.restoration_dir()) {
			if e.kind() != ErrorKind::NotFound {
				return Err(e.into())
			}
		}

		Ok(service)
	}

	// get the root path.
	fn root_dir(&self) -> PathBuf {
		let mut dir = self.db_path.clone();
		dir.push("snapshot");
		dir
	}

	// get the current snapshot dir.
	fn snapshot_dir(&self) -> PathBuf {
		let mut dir = self.root_dir();
		dir.push("current");
		dir
	}

	// get the restoration directory.
	fn restoration_dir(&self) -> PathBuf {
		let mut dir = self.root_dir();
		dir.push("restoration");
		dir
	}

	// restoration db path.
	fn restoration_db(&self) -> PathBuf {
		let mut dir = self.restoration_dir();
		dir.push("db");
		dir
	}

	// temporary snapshot recovery path.
	fn temp_recovery_dir(&self) -> PathBuf {
		let mut dir = self.restoration_dir();
		dir.push("temp");
		dir
	}

	// replace one the client's database with our own.
	fn replace_client_db(&self) -> Result<(), Error> {
		let our_db = self.restoration_db();

		trace!(target: "snapshot", "replacing {:?} with {:?}", self.client_db, our_db);

		let mut backup_db = self.restoration_dir();
		backup_db.push("backup_db");

		let _ = fs::remove_dir_all(&backup_db);

		let existed = match fs::rename(&self.client_db, &backup_db) {
			Ok(_) => true,
			Err(e) => if let ErrorKind::NotFound = e.kind() {
				false
			} else {
				return Err(e.into());
			}
		};

		match fs::rename(&our_db, &self.client_db) {
			Ok(_) => {
				// clean up the backup.
				if existed {
					try!(fs::remove_dir_all(&backup_db));
				}
				Ok(())
			}
			Err(e) => {
				// restore the backup.
				if existed {
					try!(fs::rename(&backup_db, &self.client_db));
				}
				Err(e.into())
			}
		}
	}

	/// Initialize the restoration synchronously.
	pub fn init_restore(&self, manifest: ManifestData) -> Result<(), Error> {
		let rest_dir = self.restoration_dir();

		let mut res = self.restoration.lock();

		// tear down existing restoration.
		*res = None;

		// delete and restore the restoration dir.
		if let Err(e) = fs::remove_dir_all(&rest_dir) {
			match e.kind() {
				ErrorKind::NotFound => {},
				_ => return Err(e.into()),
			}
		}

		try!(fs::create_dir_all(&rest_dir));

		// make new restoration.
		let writer = try!(LooseWriter::new(self.temp_recovery_dir()));

		let params = RestorationParams {
			manifest: manifest,
			pruning: self.pruning,
			db_path: self.restoration_db(),
			writer: writer,
			genesis: &self.genesis_block,
		};

		*res = Some(try!(Restoration::new(params)));

		*self.status.lock() = RestorationStatus::Ongoing;
		Ok(())
	}

	// finalize the restoration. this accepts an already-locked
	// restoration as an argument -- so acquiring it again _will_
	// lead to deadlock.
	fn finalize_restoration(&self, rest: &mut Option<Restoration>) -> Result<(), Error> {
		trace!(target: "snapshot", "finalizing restoration");

		self.state_chunks.store(0, Ordering::SeqCst);
		self.block_chunks.store(0, Ordering::SeqCst);

		// destroy the restoration before replacing databases and snapshot.
		try!(rest.take().map(Restoration::finalize).unwrap_or(Ok(())));
		try!(self.replace_client_db());

		let mut reader = self.reader.write();
		*reader = None; // destroy the old reader if it existed.

		let snapshot_dir = self.snapshot_dir();

		trace!(target: "snapshot", "removing old snapshot dir at {}", snapshot_dir.to_string_lossy());
		if let Err(e) = fs::remove_dir_all(&snapshot_dir) {
			match e.kind() {
				ErrorKind::NotFound => {}
				_ => return Err(e.into()),
			}
		}

		try!(fs::create_dir(&snapshot_dir));

		trace!(target: "snapshot", "copying restored snapshot files over");
		for maybe_file in try!(fs::read_dir(self.temp_recovery_dir())) {
			let path = try!(maybe_file).path();
			if let Some(name) = path.file_name().map(|x| x.to_owned()) {
				let mut new_path = snapshot_dir.clone();
				new_path.push(name);
				try!(fs::rename(path, new_path));
			}
		}

		let _ = fs::remove_dir_all(self.restoration_dir());

		*reader = Some(try!(LooseReader::new(snapshot_dir)));

		*self.status.lock() = RestorationStatus::Inactive;

		Ok(())
	}

	/// Feed a chunk of either kind. no-op if no restoration or status is wrong.
	fn feed_chunk(&self, hash: H256, chunk: &[u8], is_state: bool) -> Result<(), Error> {
		// TODO: be able to process block chunks and state chunks at same time?
		let mut restoration = self.restoration.lock();

		match self.status() {
			RestorationStatus::Inactive | RestorationStatus::Failed => Ok(()),
			RestorationStatus::Ongoing => {
				let res = {
					let rest = match *restoration {
						Some(ref mut r) => r,
						None => return Ok(()),
					};

					match is_state {
						true => rest.feed_state(hash, chunk),
						false => rest.feed_blocks(hash, chunk, &*self.engine),
					}.map(|_| rest.is_done())
				};

				match res {
					Ok(is_done) => {
						match is_state {
							true => self.state_chunks.fetch_add(1, Ordering::SeqCst),
							false => self.block_chunks.fetch_add(1, Ordering::SeqCst),
						};

						match is_done {
							true => self.finalize_restoration(&mut *restoration),
							false => Ok(())
						}
					}
					other => other.map(drop),
				}
			}
		}
	}

	/// Feed a state chunk to be processed synchronously.
	pub fn feed_state_chunk(&self, hash: H256, chunk: &[u8]) {
		match self.feed_chunk(hash, chunk, true) {
			Ok(()) => (),
			Err(e) => {
				warn!("Encountered error during state restoration: {}", e);
				*self.restoration.lock() = None;
				*self.status.lock() = RestorationStatus::Failed;
				let _ = fs::remove_dir_all(self.restoration_dir());
			}
		}
	}

	/// Feed a block chunk to be processed synchronously.
	pub fn feed_block_chunk(&self, hash: H256, chunk: &[u8]) {
		match self.feed_chunk(hash, chunk, false) {
			Ok(()) => (),
			Err(e) => {
				warn!("Encountered error during block restoration: {}", e);
				*self.restoration.lock() = None;
				*self.status.lock() = RestorationStatus::Failed;
				let _ = fs::remove_dir_all(self.restoration_dir());
			}
		}
	}
}

impl SnapshotService for Service {
	fn manifest(&self) -> Option<ManifestData> {
		self.reader.read().as_ref().map(|r| r.manifest().clone())
	}

	fn chunk(&self, hash: H256) -> Option<Bytes> {
		self.reader.read().as_ref().and_then(|r| r.chunk(hash).ok())
	}

	fn status(&self) -> RestorationStatus {
		*self.status.lock()
	}

	fn chunks_done(&self) -> (usize, usize) {
		(self.state_chunks.load(Ordering::Relaxed), self.block_chunks.load(Ordering::Relaxed))
	}

	fn begin_restore(&self, manifest: ManifestData) {
		self.io_channel.send(ClientIoMessage::BeginRestoration(manifest))
			.expect("snapshot service and io service are kept alive by client service; qed");
	}

	fn abort_restore(&self) {
		*self.restoration.lock() = None;
		*self.status.lock() = RestorationStatus::Inactive;
		if let Err(e) = fs::remove_dir_all(&self.restoration_dir()) {
			match e.kind() {
				ErrorKind::NotFound => {},
				_ => warn!("encountered error {} while deleting snapshot restoration dir.", e),
			}
		}
	}

	fn restore_state_chunk(&self, hash: H256, chunk: Bytes) {
		self.io_channel.send(ClientIoMessage::FeedStateChunk(hash, chunk))
			.expect("snapshot service and io service are kept alive by client service; qed");
	}

	fn restore_block_chunk(&self, hash: H256, chunk: Bytes) {
		self.io_channel.send(ClientIoMessage::FeedBlockChunk(hash, chunk))
			.expect("snapshot service and io service are kept alive by client service; qed");
	}
}
