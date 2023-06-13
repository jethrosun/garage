use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use base64::prelude::*;
use futures::prelude::*;
use futures::try_join;
use hyper::body::{Body, Bytes};
use hyper::header::{HeaderMap, HeaderValue};
use hyper::{Request, Response};
use md5::{digest::generic_array::*, Digest as Md5Digest, Md5};
use sha2::Sha256;

use opentelemetry::{
	trace::{FutureExt as OtelFutureExt, TraceContextExt, Tracer},
	Context,
};

use garage_rpc::netapp::bytes_buf::BytesBuf;
use garage_table::*;
use garage_util::async_hash::*;
use garage_util::data::*;
use garage_util::error::Error as GarageError;
use garage_util::time::*;

use garage_block::manager::INLINE_THRESHOLD;
use garage_model::bucket_table::Bucket;
use garage_model::garage::Garage;
use garage_model::index_counter::CountedItem;
use garage_model::s3::block_ref_table::*;
use garage_model::s3::object_table::*;
use garage_model::s3::version_table::*;

use crate::s3::error::*;
use crate::s3::xml as s3_xml;
use crate::signature::verify_signed_content;

pub async fn handle_put(
	garage: Arc<Garage>,
	req: Request<Body>,
	bucket: &Bucket,
	key: &String,
	content_sha256: Option<Hash>,
) -> Result<Response<Body>, Error> {
	// Retrieve interesting headers from request
	let headers = get_headers(req.headers())?;
	debug!("Object headers: {:?}", headers);

	let content_md5 = match req.headers().get("content-md5") {
		Some(x) => Some(x.to_str()?.to_string()),
		None => None,
	};

	let (_head, body) = req.into_parts();
	let body = body.map_err(Error::from);

	save_stream(
		garage,
		headers,
		body,
		bucket,
		key,
		content_md5,
		content_sha256,
	)
	.await
	.map(|(uuid, md5)| put_response(uuid, md5))
}

pub(crate) async fn save_stream<S: Stream<Item = Result<Bytes, Error>> + Unpin>(
	garage: Arc<Garage>,
	headers: ObjectVersionHeaders,
	body: S,
	bucket: &Bucket,
	key: &String,
	content_md5: Option<String>,
	content_sha256: Option<FixedBytes32>,
) -> Result<(Uuid, String), Error> {
	let mut chunker = StreamChunker::new(body, garage.config.block_size);
	let (first_block_opt, existing_object) = try_join!(
		chunker.next(),
		garage
			.object_table
			.get(&bucket.id, key)
			.map_err(Error::from),
	)?;

	let first_block = first_block_opt.unwrap_or_default();

	// Generate identity of new version
	let version_uuid = gen_uuid();
	let version_timestamp = existing_object
		.as_ref()
		.and_then(|obj| obj.versions().iter().map(|v| v.timestamp).max())
		.map(|t| std::cmp::max(t + 1, now_msec()))
		.unwrap_or_else(now_msec);

	// If body is small enough, store it directly in the object table
	// as "inline data". We can then return immediately.
	if first_block.len() < INLINE_THRESHOLD {
		let mut md5sum = Md5::new();
		md5sum.update(&first_block[..]);
		let data_md5sum = md5sum.finalize();
		let data_md5sum_hex = hex::encode(data_md5sum);

		let data_sha256sum = sha256sum(&first_block[..]);
		let size = first_block.len() as u64;

		ensure_checksum_matches(
			data_md5sum.as_slice(),
			data_sha256sum,
			content_md5.as_deref(),
			content_sha256,
		)?;

		check_quotas(&garage, bucket, key, size, existing_object.as_ref()).await?;

		let object_version = ObjectVersion {
			uuid: version_uuid,
			timestamp: version_timestamp,
			state: ObjectVersionState::Complete(ObjectVersionData::Inline(
				ObjectVersionMeta {
					headers,
					size,
					etag: data_md5sum_hex.clone(),
				},
				first_block.to_vec(),
			)),
		};

		let object = Object::new(bucket.id, key.into(), vec![object_version]);
		garage.object_table.insert(&object).await?;

		return Ok((version_uuid, data_md5sum_hex));
	}

	// The following consists in many steps that can each fail.
	// Keep track that some cleanup will be needed if things fail
	// before everything is finished (cleanup is done using the Drop trait).
	let mut interrupted_cleanup = InterruptedCleanup(Some((
		garage.clone(),
		bucket.id,
		key.into(),
		version_uuid,
		version_timestamp,
	)));

	// Write version identifier in object table so that we have a trace
	// that we are uploading something
	let mut object_version = ObjectVersion {
		uuid: version_uuid,
		timestamp: version_timestamp,
		state: ObjectVersionState::Uploading(headers.clone()),
	};
	let object = Object::new(bucket.id, key.into(), vec![object_version.clone()]);
	garage.object_table.insert(&object).await?;

	// Initialize corresponding entry in version table
	// Write this entry now, even with empty block list,
	// to prevent block_ref entries from being deleted (they can be deleted
	// if the reference a version that isn't found in the version table)
	let version = Version::new(version_uuid, bucket.id, key.into(), false);
	garage.version_table.insert(&version).await?;

	// Transfer data and verify checksum
	let first_block_hash = async_blake2sum(first_block.clone()).await;

	let (total_size, data_md5sum, data_sha256sum) = read_and_put_blocks(
		&garage,
		&version,
		1,
		first_block,
		first_block_hash,
		&mut chunker,
	)
	.await?;

	ensure_checksum_matches(
		data_md5sum.as_slice(),
		data_sha256sum,
		content_md5.as_deref(),
		content_sha256,
	)?;

	check_quotas(&garage, bucket, key, total_size, existing_object.as_ref()).await?;

	// Save final object state, marked as Complete
	let md5sum_hex = hex::encode(data_md5sum);
	object_version.state = ObjectVersionState::Complete(ObjectVersionData::FirstBlock(
		ObjectVersionMeta {
			headers,
			size: total_size,
			etag: md5sum_hex.clone(),
		},
		first_block_hash,
	));
	let object = Object::new(bucket.id, key.into(), vec![object_version]);
	garage.object_table.insert(&object).await?;

	// We were not interrupted, everything went fine.
	// We won't have to clean up on drop.
	interrupted_cleanup.cancel();

	Ok((version_uuid, md5sum_hex))
}

/// Validate MD5 sum against content-md5 header
/// and sha256sum against signed content-sha256
fn ensure_checksum_matches(
	data_md5sum: &[u8],
	data_sha256sum: garage_util::data::FixedBytes32,
	content_md5: Option<&str>,
	content_sha256: Option<garage_util::data::FixedBytes32>,
) -> Result<(), Error> {
	if let Some(expected_sha256) = content_sha256 {
		if expected_sha256 != data_sha256sum {
			return Err(Error::bad_request(
				"Unable to validate x-amz-content-sha256",
			));
		} else {
			trace!("Successfully validated x-amz-content-sha256");
		}
	}
	if let Some(expected_md5) = content_md5 {
		if expected_md5.trim_matches('"') != BASE64_STANDARD.encode(data_md5sum) {
			return Err(Error::bad_request("Unable to validate content-md5"));
		} else {
			trace!("Successfully validated content-md5");
		}
	}
	Ok(())
}

/// Check that inserting this object with this size doesn't exceed bucket quotas
async fn check_quotas(
	garage: &Arc<Garage>,
	bucket: &Bucket,
	key: &str,
	size: u64,
	prev_object: Option<&Object>,
) -> Result<(), Error> {
	let quotas = bucket.state.as_option().unwrap().quotas.get();
	if quotas.max_objects.is_none() && quotas.max_size.is_none() {
		return Ok(());
	};

	let key = key.to_string();
	let counters = garage
		.object_counter_table
		.table
		.get(&bucket.id, &EmptyKey)
		.await?;

	let counters = counters
		.map(|x| x.filtered_values(&garage.system.ring.borrow()))
		.unwrap_or_default();

	let (prev_cnt_obj, prev_cnt_size) = match prev_object {
		Some(o) => {
			let prev_cnt = o.counts().into_iter().collect::<HashMap<_, _>>();
			(
				prev_cnt.get(OBJECTS).cloned().unwrap_or_default(),
				prev_cnt.get(BYTES).cloned().unwrap_or_default(),
			)
		}
		None => (0, 0),
	};
	let cnt_obj_diff = 1 - prev_cnt_obj;
	let cnt_size_diff = size as i64 - prev_cnt_size;

	if let Some(mo) = quotas.max_objects {
		let current_objects = counters.get(OBJECTS).cloned().unwrap_or_default();
		if cnt_obj_diff > 0 && current_objects + cnt_obj_diff > mo as i64 {
			return Err(Error::forbidden(format!(
				"Object quota is reached, maximum objects for this bucket: {}",
				mo
			)));
		}
	}

	if let Some(ms) = quotas.max_size {
		let current_size = counters.get(BYTES).cloned().unwrap_or_default();
		if cnt_size_diff > 0 && current_size + cnt_size_diff > ms as i64 {
			return Err(Error::forbidden(format!(
				"Bucket size quota is reached, maximum total size of objects for this bucket: {}. The bucket is already {} bytes, and this object would add {} bytes.",
				ms, current_size, cnt_size_diff
			)));
		}
	}

	Ok(())
}

async fn read_and_put_blocks<S: Stream<Item = Result<Bytes, Error>> + Unpin>(
	garage: &Garage,
	version: &Version,
	part_number: u64,
	first_block: Bytes,
	first_block_hash: Hash,
	chunker: &mut StreamChunker<S>,
) -> Result<(u64, GenericArray<u8, typenum::U16>, Hash), Error> {
	let tracer = opentelemetry::global::tracer("garage");

	let md5hasher = AsyncHasher::<Md5>::new();
	let sha256hasher = AsyncHasher::<Sha256>::new();

	futures::future::join(
		md5hasher.update(first_block.clone()),
		sha256hasher.update(first_block.clone()),
	)
	.with_context(Context::current_with_span(
		tracer.start("Hash first block (md5, sha256)"),
	))
	.await;

	let mut next_offset = first_block.len();
	let mut put_curr_version_block = put_block_meta(
		garage,
		version,
		part_number,
		0,
		first_block_hash,
		first_block.len() as u64,
	);
	let mut put_curr_block = garage
		.block_manager
		.rpc_put_block(first_block_hash, first_block);

	loop {
		let (_, _, next_block) = futures::try_join!(
			put_curr_block.map_err(Error::from),
			put_curr_version_block.map_err(Error::from),
			chunker.next(),
		)?;
		if let Some(block) = next_block {
			let (_, _, block_hash) = futures::future::join3(
				md5hasher.update(block.clone()),
				sha256hasher.update(block.clone()),
				async_blake2sum(block.clone()),
			)
			.with_context(Context::current_with_span(
				tracer.start("Hash block (md5, sha256, blake2)"),
			))
			.await;
			let block_len = block.len();
			put_curr_version_block = put_block_meta(
				garage,
				version,
				part_number,
				next_offset as u64,
				block_hash,
				block_len as u64,
			);
			put_curr_block = garage.block_manager.rpc_put_block(block_hash, block);
			next_offset += block_len;
		} else {
			break;
		}
	}

	let total_size = next_offset as u64;
	let data_md5sum = md5hasher.finalize().await;

	let data_sha256sum = sha256hasher.finalize().await;
	let data_sha256sum = Hash::try_from(&data_sha256sum[..]).unwrap();

	Ok((total_size, data_md5sum, data_sha256sum))
}

async fn put_block_meta(
	garage: &Garage,
	version: &Version,
	part_number: u64,
	offset: u64,
	hash: Hash,
	size: u64,
) -> Result<(), GarageError> {
	let mut version = version.clone();
	version.blocks.put(
		VersionBlockKey {
			part_number,
			offset,
		},
		VersionBlock { hash, size },
	);

	let block_ref = BlockRef {
		block: hash,
		version: version.uuid,
		deleted: false.into(),
	};

	futures::try_join!(
		garage.version_table.insert(&version),
		garage.block_ref_table.insert(&block_ref),
	)?;
	Ok(())
}

struct StreamChunker<S: Stream<Item = Result<Bytes, Error>>> {
	stream: S,
	read_all: bool,
	block_size: usize,
	buf: BytesBuf,
}

impl<S: Stream<Item = Result<Bytes, Error>> + Unpin> StreamChunker<S> {
	fn new(stream: S, block_size: usize) -> Self {
		Self {
			stream,
			read_all: false,
			block_size,
			buf: BytesBuf::new(),
		}
	}

	async fn next(&mut self) -> Result<Option<Bytes>, Error> {
		while !self.read_all && self.buf.len() < self.block_size {
			if let Some(block) = self.stream.next().await {
				let bytes = block?;
				trace!("Body next: {} bytes", bytes.len());
				self.buf.extend(bytes);
			} else {
				self.read_all = true;
			}
		}

		if self.buf.is_empty() {
			Ok(None)
		} else {
			Ok(Some(self.buf.take_max(self.block_size)))
		}
	}
}

pub fn put_response(version_uuid: Uuid, md5sum_hex: String) -> Response<Body> {
	Response::builder()
		.header("x-amz-version-id", hex::encode(version_uuid))
		.header("ETag", format!("\"{}\"", md5sum_hex))
		.body(Body::from(vec![]))
		.unwrap()
}

struct InterruptedCleanup(Option<(Arc<Garage>, Uuid, String, Uuid, u64)>);

impl InterruptedCleanup {
	fn cancel(&mut self) {
		drop(self.0.take());
	}
}
impl Drop for InterruptedCleanup {
	fn drop(&mut self) {
		if let Some((garage, bucket_id, key, version_uuid, version_ts)) = self.0.take() {
			tokio::spawn(async move {
				let object_version = ObjectVersion {
					uuid: version_uuid,
					timestamp: version_ts,
					state: ObjectVersionState::Aborted,
				};
				let object = Object::new(bucket_id, key, vec![object_version]);
				if let Err(e) = garage.object_table.insert(&object).await {
					warn!("Cannot cleanup after aborted PutObject: {}", e);
				}
			});
		}
	}
}

// ----

pub async fn handle_create_multipart_upload(
	garage: Arc<Garage>,
	req: &Request<Body>,
	bucket_name: &str,
	bucket_id: Uuid,
	key: &str,
) -> Result<Response<Body>, Error> {
	let version_uuid = gen_uuid();
	let headers = get_headers(req.headers())?;

	// Create object in object table
	let object_version = ObjectVersion {
		uuid: version_uuid,
		timestamp: now_msec(),
		state: ObjectVersionState::Uploading(headers),
	};
	let object = Object::new(bucket_id, key.to_string(), vec![object_version]);
	garage.object_table.insert(&object).await?;

	// Insert empty version so that block_ref entries refer to something
	// (they are inserted concurrently with blocks in the version table, so
	// there is the possibility that they are inserted before the version table
	// is created, in which case it is allowed to delete them, e.g. in repair_*)
	let version = Version::new(version_uuid, bucket_id, key.into(), false);
	garage.version_table.insert(&version).await?;

	// Send success response
	let result = s3_xml::InitiateMultipartUploadResult {
		xmlns: (),
		bucket: s3_xml::Value(bucket_name.to_string()),
		key: s3_xml::Value(key.to_string()),
		upload_id: s3_xml::Value(hex::encode(version_uuid)),
	};
	let xml = s3_xml::to_xml_with_header(&result)?;

	Ok(Response::new(Body::from(xml.into_bytes())))
}

pub async fn handle_put_part(
	garage: Arc<Garage>,
	req: Request<Body>,
	bucket_id: Uuid,
	key: &str,
	part_number: u64,
	upload_id: &str,
	content_sha256: Option<Hash>,
) -> Result<Response<Body>, Error> {
	let version_uuid = decode_upload_id(upload_id)?;

	let content_md5 = match req.headers().get("content-md5") {
		Some(x) => Some(x.to_str()?.to_string()),
		None => None,
	};

	// Read first chuck, and at the same time try to get object to see if it exists
	let key = key.to_string();

	let body = req.into_body().map_err(Error::from);
	let mut chunker = StreamChunker::new(body, garage.config.block_size);

	let (object, version, first_block) = futures::try_join!(
		garage
			.object_table
			.get(&bucket_id, &key)
			.map_err(Error::from),
		garage
			.version_table
			.get(&version_uuid, &EmptyKey)
			.map_err(Error::from),
		chunker.next(),
	)?;

	// Check object is valid and multipart block can be accepted
	let first_block = first_block.ok_or_bad_request("Empty body")?;
	let object = object.ok_or_bad_request("Object not found")?;

	if !object
		.versions()
		.iter()
		.any(|v| v.uuid == version_uuid && v.is_uploading())
	{
		return Err(Error::NoSuchUpload);
	}

	// Check part hasn't already been uploaded
	if let Some(v) = version {
		if v.has_part_number(part_number) {
			return Err(Error::bad_request(format!(
				"Part number {} has already been uploaded",
				part_number
			)));
		}
	}

	// Copy block to store
	let version = Version::new(version_uuid, bucket_id, key, false);

	let first_block_hash = async_blake2sum(first_block.clone()).await;

	let (_, data_md5sum, data_sha256sum) = read_and_put_blocks(
		&garage,
		&version,
		part_number,
		first_block,
		first_block_hash,
		&mut chunker,
	)
	.await?;

	// Verify that checksums map
	ensure_checksum_matches(
		data_md5sum.as_slice(),
		data_sha256sum,
		content_md5.as_deref(),
		content_sha256,
	)?;

	// Store part etag in version
	let data_md5sum_hex = hex::encode(data_md5sum);
	let mut version = version;
	version
		.parts_etags
		.put(part_number, data_md5sum_hex.clone());
	garage.version_table.insert(&version).await?;

	let response = Response::builder()
		.header("ETag", format!("\"{}\"", data_md5sum_hex))
		.body(Body::empty())
		.unwrap();
	Ok(response)
}

pub async fn handle_complete_multipart_upload(
	garage: Arc<Garage>,
	req: Request<Body>,
	bucket_name: &str,
	bucket: &Bucket,
	key: &str,
	upload_id: &str,
	content_sha256: Option<Hash>,
) -> Result<Response<Body>, Error> {
	let body = hyper::body::to_bytes(req.into_body()).await?;

	if let Some(content_sha256) = content_sha256 {
		verify_signed_content(content_sha256, &body[..])?;
	}

	let body_xml = roxmltree::Document::parse(std::str::from_utf8(&body)?)?;
	let body_list_of_parts = parse_complete_multipart_upload_body(&body_xml)
		.ok_or_bad_request("Invalid CompleteMultipartUpload XML")?;
	debug!(
		"CompleteMultipartUpload list of parts: {:?}",
		body_list_of_parts
	);

	let version_uuid = decode_upload_id(upload_id)?;

	// Get object and version
	let key = key.to_string();
	let (object, version) = futures::try_join!(
		garage.object_table.get(&bucket.id, &key),
		garage.version_table.get(&version_uuid, &EmptyKey),
	)?;

	let object = object.ok_or(Error::NoSuchKey)?;
	let mut object_version = object
		.versions()
		.iter()
		.find(|v| v.uuid == version_uuid && v.is_uploading())
		.cloned()
		.ok_or(Error::NoSuchUpload)?;

	let version = version.ok_or(Error::NoSuchKey)?;
	if version.blocks.is_empty() {
		return Err(Error::bad_request("No data was uploaded"));
	}

	let headers = match object_version.state {
		ObjectVersionState::Uploading(headers) => headers,
		_ => unreachable!(),
	};

	// Check that part numbers are an increasing sequence.
	// (it doesn't need to start at 1 nor to be a continuous sequence,
	// see discussion in #192)
	if body_list_of_parts.is_empty() {
		return Err(Error::EntityTooSmall);
	}
	if !body_list_of_parts
		.iter()
		.zip(body_list_of_parts.iter().skip(1))
		.all(|(p1, p2)| p1.part_number < p2.part_number)
	{
		return Err(Error::InvalidPartOrder);
	}

	// Garage-specific restriction, see #204: part numbers must be
	// consecutive starting at 1
	if body_list_of_parts[0].part_number != 1
		|| !body_list_of_parts
			.iter()
			.zip(body_list_of_parts.iter().skip(1))
			.all(|(p1, p2)| p1.part_number + 1 == p2.part_number)
	{
		return Err(Error::NotImplemented("Garage does not support completing a Multipart upload with non-consecutive part numbers. This is a restriction of Garage's data model, which might be fixed in a future release. See issue #204 for more information on this topic.".into()));
	}

	// Check that the list of parts they gave us corresponds to the parts we have here
	debug!("Expected parts from request: {:?}", body_list_of_parts);
	debug!("Parts stored in version: {:?}", version.parts_etags.items());
	let parts = version
		.parts_etags
		.items()
		.iter()
		.map(|pair| (&pair.0, &pair.1));
	let same_parts = body_list_of_parts
		.iter()
		.map(|x| (&x.part_number, &x.etag))
		.eq(parts);
	if !same_parts {
		return Err(Error::InvalidPart);
	}

	// Check that all blocks belong to one of the parts
	let block_parts = version
		.blocks
		.items()
		.iter()
		.map(|(bk, _)| bk.part_number)
		.collect::<BTreeSet<_>>();
	let same_parts = body_list_of_parts
		.iter()
		.map(|x| x.part_number)
		.eq(block_parts.into_iter());
	if !same_parts {
		return Err(Error::bad_request(
			"Part numbers in block list and part list do not match. This can happen if a part was partially uploaded. Please abort the multipart upload and try again."
		));
	}

	// Calculate etag of final object
	// To understand how etags are calculated, read more here:
	// https://teppen.io/2018/06/23/aws_s3_etags/
	let num_parts = body_list_of_parts.len();
	let mut etag_md5_hasher = Md5::new();
	for (_, etag) in version.parts_etags.items().iter() {
		etag_md5_hasher.update(etag.as_bytes());
	}
	let etag = format!("{}-{}", hex::encode(etag_md5_hasher.finalize()), num_parts);

	// Calculate total size of final object
	let total_size = version.blocks.items().iter().map(|x| x.1.size).sum();

	if let Err(e) = check_quotas(&garage, bucket, &key, total_size, Some(&object)).await {
		object_version.state = ObjectVersionState::Aborted;
		let final_object = Object::new(bucket.id, key.clone(), vec![object_version]);
		garage.object_table.insert(&final_object).await?;

		return Err(e);
	}

	// Write final object version
	object_version.state = ObjectVersionState::Complete(ObjectVersionData::FirstBlock(
		ObjectVersionMeta {
			headers,
			size: total_size,
			etag: etag.clone(),
		},
		version.blocks.items()[0].1.hash,
	));

	let final_object = Object::new(bucket.id, key.clone(), vec![object_version]);
	garage.object_table.insert(&final_object).await?;

	// Send response saying ok we're done
	let result = s3_xml::CompleteMultipartUploadResult {
		xmlns: (),
		location: None,
		bucket: s3_xml::Value(bucket_name.to_string()),
		key: s3_xml::Value(key),
		etag: s3_xml::Value(format!("\"{}\"", etag)),
	};
	let xml = s3_xml::to_xml_with_header(&result)?;

	Ok(Response::new(Body::from(xml.into_bytes())))
}

pub async fn handle_abort_multipart_upload(
	garage: Arc<Garage>,
	bucket_id: Uuid,
	key: &str,
	upload_id: &str,
) -> Result<Response<Body>, Error> {
	let version_uuid = decode_upload_id(upload_id)?;

	let object = garage
		.object_table
		.get(&bucket_id, &key.to_string())
		.await?;
	let object = object.ok_or(Error::NoSuchKey)?;

	let object_version = object
		.versions()
		.iter()
		.find(|v| v.uuid == version_uuid && v.is_uploading());
	let mut object_version = match object_version {
		None => return Err(Error::NoSuchUpload),
		Some(x) => x.clone(),
	};

	object_version.state = ObjectVersionState::Aborted;
	let final_object = Object::new(bucket_id, key.to_string(), vec![object_version]);
	garage.object_table.insert(&final_object).await?;

	Ok(Response::new(Body::from(vec![])))
}

fn get_mime_type(headers: &HeaderMap<HeaderValue>) -> Result<String, Error> {
	Ok(headers
		.get(hyper::header::CONTENT_TYPE)
		.map(|x| x.to_str())
		.unwrap_or(Ok("blob"))?
		.to_string())
}

pub(crate) fn get_headers(headers: &HeaderMap<HeaderValue>) -> Result<ObjectVersionHeaders, Error> {
	let content_type = get_mime_type(headers)?;
	let mut other = BTreeMap::new();

	// Preserve standard headers
	let standard_header = vec![
		hyper::header::CACHE_CONTROL,
		hyper::header::CONTENT_DISPOSITION,
		hyper::header::CONTENT_ENCODING,
		hyper::header::CONTENT_LANGUAGE,
		hyper::header::EXPIRES,
	];
	for h in standard_header.iter() {
		if let Some(v) = headers.get(h) {
			match v.to_str() {
				Ok(v_str) => {
					other.insert(h.to_string(), v_str.to_string());
				}
				Err(e) => {
					warn!("Discarding header {}, error in .to_str(): {}", h, e);
				}
			}
		}
	}

	// Preserve x-amz-meta- headers
	for (k, v) in headers.iter() {
		if k.as_str().starts_with("x-amz-meta-") {
			match v.to_str() {
				Ok(v_str) => {
					other.insert(k.to_string(), v_str.to_string());
				}
				Err(e) => {
					warn!("Discarding header {}, error in .to_str(): {}", k, e);
				}
			}
		}
	}

	Ok(ObjectVersionHeaders {
		content_type,
		other,
	})
}

pub fn decode_upload_id(id: &str) -> Result<Uuid, Error> {
	let id_bin = hex::decode(id).map_err(|_| Error::NoSuchUpload)?;
	if id_bin.len() != 32 {
		return Err(Error::NoSuchUpload);
	}
	let mut uuid = [0u8; 32];
	uuid.copy_from_slice(&id_bin[..]);
	Ok(Uuid::from(uuid))
}

#[derive(Debug)]
struct CompleteMultipartUploadPart {
	etag: String,
	part_number: u64,
}

fn parse_complete_multipart_upload_body(
	xml: &roxmltree::Document,
) -> Option<Vec<CompleteMultipartUploadPart>> {
	let mut parts = vec![];

	let root = xml.root();
	let cmu = root.first_child()?;
	if !cmu.has_tag_name("CompleteMultipartUpload") {
		return None;
	}

	for item in cmu.children() {
		// Only parse <Part> nodes
		if !item.is_element() {
			continue;
		}

		if item.has_tag_name("Part") {
			let etag = item.children().find(|e| e.has_tag_name("ETag"))?.text()?;
			let part_number = item
				.children()
				.find(|e| e.has_tag_name("PartNumber"))?
				.text()?;
			parts.push(CompleteMultipartUploadPart {
				etag: etag.trim_matches('"').to_string(),
				part_number: part_number.parse().ok()?,
			});
		} else {
			return None;
		}
	}

	Some(parts)
}
