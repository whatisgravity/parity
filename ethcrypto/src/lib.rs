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

//! Crypto utils used ethstore and network.

extern crate bigint;
extern crate tiny_keccak;
extern crate crypto as rcrypto;
extern crate secp256k1;
extern crate ethkey;

use tiny_keccak::Keccak;
use rcrypto::pbkdf2::pbkdf2;
use rcrypto::scrypt::{scrypt, ScryptParams};
use rcrypto::sha2::Sha256;
use rcrypto::hmac::Hmac;
use secp256k1::Error as SecpError;

pub const KEY_LENGTH: usize = 32;
pub const KEY_ITERATIONS: usize = 10240;
pub const KEY_LENGTH_AES: usize = KEY_LENGTH / 2;

#[derive(PartialEq, Debug)]
pub enum Error {
	Secp(SecpError),
	InvalidMessage,
}

impl From<SecpError> for Error {
	fn from(e: SecpError) -> Self {
		Error::Secp(e)
	}
}

pub trait Keccak256<T> {
	fn keccak256(&self) -> T where T: Sized;
}

impl Keccak256<[u8; 32]> for [u8] {
	fn keccak256(&self) -> [u8; 32] {
		let mut keccak = Keccak::new_keccak256();
		let mut result = [0u8; 32];
		keccak.update(self);
		keccak.finalize(&mut result);
		result
	}
}

pub fn derive_key_iterations(password: &str, salt: &[u8; 32], c: u32) -> (Vec<u8>, Vec<u8>) {
	let mut h_mac = Hmac::new(Sha256::new(), password.as_bytes());
	let mut derived_key = vec![0u8; KEY_LENGTH];
	pbkdf2(&mut h_mac, salt, c, &mut derived_key);
	let derived_right_bits = &derived_key[0..KEY_LENGTH_AES];
	let derived_left_bits = &derived_key[KEY_LENGTH_AES..KEY_LENGTH];
	(derived_right_bits.to_vec(), derived_left_bits.to_vec())
}

pub fn derive_key_scrypt(password: &str, salt: &[u8; 32], n: u32, p: u32, r: u32) -> (Vec<u8>, Vec<u8>) {
	let mut derived_key = vec![0u8; KEY_LENGTH];
	let scrypt_params = ScryptParams::new(n.trailing_zeros() as u8, r, p);
	scrypt(password.as_bytes(), salt, &scrypt_params, &mut derived_key);
	let derived_right_bits = &derived_key[0..KEY_LENGTH_AES];
	let derived_left_bits = &derived_key[KEY_LENGTH_AES..KEY_LENGTH];
	(derived_right_bits.to_vec(), derived_left_bits.to_vec())
}

pub fn derive_mac(derived_left_bits: &[u8], cipher_text: &[u8]) -> Vec<u8> {
	let mut mac = vec![0u8; KEY_LENGTH_AES + cipher_text.len()];
	mac[0..KEY_LENGTH_AES].copy_from_slice(derived_left_bits);
	mac[KEY_LENGTH_AES..cipher_text.len() + KEY_LENGTH_AES].copy_from_slice(cipher_text);
	mac
}

/// AES encryption
pub mod aes {
	use rcrypto::blockmodes::{CtrMode, CbcDecryptor, PkcsPadding};
	use rcrypto::aessafe::{AesSafe128Encryptor, AesSafe128Decryptor};
	use rcrypto::symmetriccipher::{Encryptor, Decryptor, SymmetricCipherError};
	use rcrypto::buffer::{RefReadBuffer, RefWriteBuffer, WriteBuffer};

	/// Encrypt a message
	pub fn encrypt(k: &[u8], iv: &[u8], plain: &[u8], dest: &mut [u8]) {
		let mut encryptor = CtrMode::new(AesSafe128Encryptor::new(k), iv.to_vec());
		encryptor.encrypt(&mut RefReadBuffer::new(plain), &mut RefWriteBuffer::new(dest), true).expect("Invalid length or padding");
	}

	/// Decrypt a message
	pub fn decrypt(k: &[u8], iv: &[u8], encrypted: &[u8], dest: &mut [u8]) {
		let mut encryptor = CtrMode::new(AesSafe128Encryptor::new(k), iv.to_vec());
		encryptor.decrypt(&mut RefReadBuffer::new(encrypted), &mut RefWriteBuffer::new(dest), true).expect("Invalid length or padding");
	}


	/// Decrypt a message using cbc mode
	pub fn decrypt_cbc(k: &[u8], iv: &[u8], encrypted: &[u8], dest: &mut [u8]) -> Result<usize, SymmetricCipherError> {
		let mut encryptor = CbcDecryptor::new(AesSafe128Decryptor::new(k), PkcsPadding, iv.to_vec());
		let len = dest.len();
		let mut buffer = RefWriteBuffer::new(dest);
		try!(encryptor.decrypt(&mut RefReadBuffer::new(encrypted), &mut buffer, true));
		Ok(len - buffer.remaining())
	}
}

/// ECDH functions
#[cfg_attr(feature="dev", allow(similar_names))]
pub mod ecdh {
	use secp256k1::{ecdh, key};
	use ethkey::{Secret, Public, SECP256K1};
	use Error;

	/// Agree on a shared secret
	pub fn agree(secret: &Secret, public: &Public) -> Result<Secret, Error> {
		let context = &SECP256K1;
		let pdata = {
			let mut temp = [4u8; 65];
			(&mut temp[1..65]).copy_from_slice(&public[0..64]);
			temp
		};

		let publ = try!(key::PublicKey::from_slice(context, &pdata));
		// no way to create SecretKey from raw byte array.
		let sec: &key::SecretKey = unsafe { ::std::mem::transmute(secret) };
		let shared = ecdh::SharedSecret::new_raw(context, &publ, sec);

		let mut s = Secret::default();
		s.copy_from_slice(&shared[0..32]);
		Ok(s)
	}
}

/// ECIES function
#[cfg_attr(feature="dev", allow(similar_names))]
pub mod ecies {
	use rcrypto::digest::Digest;
	use rcrypto::sha2::Sha256;
	use rcrypto::hmac::Hmac;
	use rcrypto::mac::Mac;
	use bigint::hash::{FixedHash, H128};
	use ethkey::{Random, Generator, Public, Secret};
	use {Error, ecdh, aes, Keccak256};

	/// Encrypt a message with a public key
	pub fn encrypt(public: &Public, shared_mac: &[u8], plain: &[u8]) -> Result<Vec<u8>, Error> {
		let r = Random.generate().unwrap();
		let z = try!(ecdh::agree(r.secret(), public));
		let mut key = [0u8; 32];
		let mut mkey = [0u8; 32];
		kdf(&z, &[0u8; 0], &mut key);
		let mut hasher = Sha256::new();
		let mkey_material = &key[16..32];
		hasher.input(mkey_material);
		hasher.result(&mut mkey);
		let ekey = &key[0..16];

		let mut msg = vec![0u8; (1 + 64 + 16 + plain.len() + 32)];
		msg[0] = 0x04u8;
		{
			let msgd = &mut msg[1..];
			msgd[0..64].copy_from_slice(r.public());
			let iv = H128::random();
			msgd[64..80].copy_from_slice(&iv);
			{
				let cipher = &mut msgd[(64 + 16)..(64 + 16 + plain.len())];
				aes::encrypt(ekey, &iv, plain, cipher);
			}
			let mut hmac = Hmac::new(Sha256::new(), &mkey);
			{
				let cipher_iv = &msgd[64..(64 + 16 + plain.len())];
				hmac.input(cipher_iv);
			}
			hmac.input(shared_mac);
			hmac.raw_result(&mut msgd[(64 + 16 + plain.len())..]);
		}
		Ok(msg)
	}

	/// Encrypt a message with a public key
	pub fn encrypt_single_message(public: &Public, plain: &[u8]) -> Result<Vec<u8>, Error> {
		let r = Random.generate().unwrap();
		let z = try!(ecdh::agree(r.secret(), public));
		let mut key = [0u8; 32];
		let mut mkey = [0u8; 32];
		kdf(&z, &[0u8; 0], &mut key);
		let mut hasher = Sha256::new();
		let mkey_material = &key[16..32];
		hasher.input(mkey_material);
		hasher.result(&mut mkey);
		let ekey = &key[0..16];

		let mut msgd = vec![0u8; (64 + plain.len())];
		{
			r.public().copy_to(&mut msgd[0..64]);
			let iv = H128::from_slice(&z.keccak256()[0..16]);
			{
				let cipher = &mut msgd[64..(64 + plain.len())];
				aes::encrypt(ekey, &iv, plain, cipher);
			}
		}
		Ok(msgd)
	}

	/// Decrypt a message with a secret key
	pub fn decrypt(secret: &Secret, shared_mac: &[u8], encrypted: &[u8]) -> Result<Vec<u8>, Error> {
		let meta_len = 1 + 64 + 16 + 32;
		if encrypted.len() < meta_len  || encrypted[0] < 2 || encrypted[0] > 4 {
			return Err(Error::InvalidMessage); //invalid message: publickey
		}

		let e = &encrypted[1..];
		let p = Public::from_slice(&e[0..64]);
		let z = try!(ecdh::agree(secret, &p));
		let mut key = [0u8; 32];
		kdf(&z, &[0u8; 0], &mut key);
		let ekey = &key[0..16];
		let mkey_material = &key[16..32];
		let mut hasher = Sha256::new();
		let mut mkey = [0u8; 32];
		hasher.input(mkey_material);
		hasher.result(&mut mkey);

		let clen = encrypted.len() - meta_len;
		let cipher_with_iv = &e[64..(64+16+clen)];
		let cipher_iv = &cipher_with_iv[0..16];
		let cipher_no_iv = &cipher_with_iv[16..];
		let msg_mac = &e[(64+16+clen)..];

		// Verify tag
		let mut hmac = Hmac::new(Sha256::new(), &mkey);
		hmac.input(cipher_with_iv);
		hmac.input(shared_mac);
		let mut mac = [0u8; 32];
		hmac.raw_result(&mut mac);
		if &mac[..] != msg_mac {
			return Err(Error::InvalidMessage);
		}

		let mut msg = vec![0u8; clen];
		aes::decrypt(ekey, cipher_iv, cipher_no_iv, &mut msg[..]);
		Ok(msg)
	}

	/// Decrypt single message with a secret key
	pub fn decrypt_single_message(secret: &Secret, encrypted: &[u8]) -> Result<Vec<u8>, Error> {
		let meta_len = 64;
		if encrypted.len() < meta_len {
			return Err(Error::InvalidMessage); //invalid message: publickey
		}

		let e = encrypted;
		let p = Public::from_slice(&e[0..64]);
		let z = try!(ecdh::agree(secret, &p));
		let mut key = [0u8; 32];
		kdf(&z, &[0u8; 0], &mut key);
		let ekey = &key[0..16];
		let mkey_material = &key[16..32];
		let mut hasher = Sha256::new();
		let mut mkey = [0u8; 32];
		hasher.input(mkey_material);
		hasher.result(&mut mkey);

		let clen = encrypted.len() - meta_len;
		let cipher = &e[64..(64+clen)];
		let mut msg = vec![0u8; clen];
		let iv = H128::from_slice(&z.keccak256()[0..16]);
		aes::decrypt(ekey, &iv, cipher, &mut msg[..]);
		Ok(msg)
	}

	fn kdf(secret: &Secret, s1: &[u8], dest: &mut [u8]) {
		let mut hasher = Sha256::new();
		// SEC/ISO/Shoup specify counter size SHOULD be equivalent
		// to size of hash output, however, it also notes that
		// the 4 bytes is okay. NIST specifies 4 bytes.
		let mut ctr = 1u32;
		let mut written = 0usize;
		while written < dest.len() {
			let ctrs = [(ctr >> 24) as u8, (ctr >> 16) as u8, (ctr >> 8) as u8, ctr as u8];
			hasher.input(&ctrs);
			hasher.input(secret);
			hasher.input(s1);
			hasher.result(&mut dest[written..(written + 32)]);
			hasher.reset();
			written += 32;
			ctr += 1;
		}
	}
}

#[cfg(test)]
mod tests {
	use ethkey::{Random, Generator};
	use ecies;

	#[test]
	fn ecies_shared() {
		let kp = Random.generate().unwrap();
		let message = b"So many books, so little time";

		let shared = b"shared";
		let wrong_shared = b"incorrect";
		let encrypted = ecies::encrypt(kp.public(), shared, message).unwrap();
		assert!(encrypted[..] != message[..]);
		assert_eq!(encrypted[0], 0x04);

		assert!(ecies::decrypt(kp.secret(), wrong_shared, &encrypted).is_err());
		let decrypted = ecies::decrypt(kp.secret(), shared, &encrypted).unwrap();
		assert_eq!(decrypted[..message.len()], message[..]);
	}

	#[test]
	fn ecies_shared_single() {
		let kp = Random.generate().unwrap();
		let message = b"So many books, so little time";
		let encrypted = ecies::encrypt_single_message(kp.public(), message).unwrap();
		assert!(encrypted[..] != message[..]);
		let decrypted = ecies::decrypt_single_message(kp.secret(), &encrypted).unwrap();
		assert_eq!(decrypted[..message.len()], message[..]);
	}
}

