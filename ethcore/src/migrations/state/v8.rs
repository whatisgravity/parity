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

//! This migration compresses the state db.

use util::migration::SimpleMigration;
use util::rlp::{Compressible, UntrustedRlp, View};

/// Compressing migration.
#[derive(Default)]
pub struct V8;

impl SimpleMigration for V8 {
	fn version(&self) -> u32 {
		8
	}

	fn simple_migrate(&mut self, key: Vec<u8>, value: Vec<u8>) -> Option<(Vec<u8>, Vec<u8>)> {
		Some((key,
					match UntrustedRlp::new(&value).compress() {
						Some(r) => r.to_vec(),
						None => value,
					}))
	}
}