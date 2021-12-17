use serde::{Deserialize, Serialize};

use garage_table::crdt::*;
use garage_table::*;
use garage_util::data::*;

/// The bucket alias table holds the names given to buckets
/// in the global namespace.
#[derive(PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct BucketAlias {
	name: String,
	pub state: crdt::Lww<crdt::Deletable<AliasParams>>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug, Serialize, Deserialize)]
pub struct AliasParams {
	pub bucket_id: Uuid,
}

impl AutoCrdt for AliasParams {
	const WARN_IF_DIFFERENT: bool = true;
}

impl BucketAlias {
	pub fn new(name: String, bucket_id: Uuid) -> Option<Self> {
		if !is_valid_bucket_name(&name) {
			None
		} else {
			Some(BucketAlias {
				name,
				state: crdt::Lww::new(crdt::Deletable::present(AliasParams { bucket_id })),
			})
		}
	}
	pub fn is_deleted(&self) -> bool {
		self.state.get().is_deleted()
	}
	pub fn name(&self) -> &str {
		&self.name
	}
}

impl Crdt for BucketAlias {
	fn merge(&mut self, o: &Self) {
		self.state.merge(&o.state);
	}
}

impl Entry<EmptyKey, String> for BucketAlias {
	fn partition_key(&self) -> &EmptyKey {
		&EmptyKey
	}
	fn sort_key(&self) -> &String {
		&self.name
	}
}

pub struct BucketAliasTable;

impl TableSchema for BucketAliasTable {
	const TABLE_NAME: &'static str = "bucket_alias";

	type P = EmptyKey;
	type S = String;
	type E = BucketAlias;
	type Filter = DeletedFilter;

	fn matches_filter(entry: &Self::E, filter: &Self::Filter) -> bool {
		filter.apply(entry.is_deleted())
	}
}

/// Check if a bucket name is valid.
///
/// The requirements are listed here:
///
/// <https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html>
///
/// In the case of Garage, bucket names must not be hex-encoded
/// 32 byte string, which is excluded thanks to the
/// maximum length of 63 bytes given in the spec.
pub fn is_valid_bucket_name(n: &str) -> bool {
	// Bucket names must be between 3 and 63 characters
	n.len() >= 3 && n.len() <= 63
	// Bucket names must be composed of lowercase letters, numbers,
	// dashes and dots
	&& n.chars().all(|c| matches!(c, '.' | '-' | 'a'..='z' | '0'..='9'))
	//  Bucket names must start and end with a letter or a number
	&& !n.starts_with(&['-', '.'][..])
	&& !n.ends_with(&['-', '.'][..])
	// Bucket names must not be formated as an IP address
	&& n.parse::<std::net::IpAddr>().is_err()
	// Bucket names must not start wih "xn--"
	&& !n.starts_with("xn--")
	// Bucket names must not end with "-s3alias"
	&& !n.ends_with("-s3alias")
}
