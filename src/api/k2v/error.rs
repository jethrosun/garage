use err_derive::Error;
use hyper::header::HeaderValue;
use hyper::{Body, HeaderMap, StatusCode};

use garage_model::helper::error::Error as HelperError;

use crate::common_error::CommonError;
pub use crate::common_error::{CommonErrorDerivative, OkOrBadRequest, OkOrInternalError};
use crate::generic_server::ApiError;
use crate::signature::error::Error as SignatureError;

/// Errors of this crate
#[derive(Debug, Error)]
pub enum Error {
	#[error(display = "{}", _0)]
	/// Error from common error
	CommonError(CommonError),

	// Category: cannot process
	/// Authorization Header Malformed
	#[error(display = "Authorization header malformed, expected scope: {}", _0)]
	AuthorizationHeaderMalformed(String),

	/// The object requested don't exists
	#[error(display = "Key not found")]
	NoSuchKey,

	/// Some base64 encoded data was badly encoded
	#[error(display = "Invalid base64: {}", _0)]
	InvalidBase64(#[error(source)] base64::DecodeError),

	/// The client sent a header with invalid value
	#[error(display = "Invalid header value: {}", _0)]
	InvalidHeader(#[error(source)] hyper::header::ToStrError),

	/// The client asked for an invalid return format (invalid Accept header)
	#[error(display = "Not acceptable: {}", _0)]
	NotAcceptable(String),

	/// The request contained an invalid UTF-8 sequence in its path or in other parameters
	#[error(display = "Invalid UTF-8: {}", _0)]
	InvalidUtf8Str(#[error(source)] std::str::Utf8Error),
}

impl<T> From<T> for Error
where
	CommonError: From<T>,
{
	fn from(err: T) -> Self {
		Error::CommonError(CommonError::from(err))
	}
}

impl CommonErrorDerivative for Error {}

impl From<HelperError> for Error {
	fn from(err: HelperError) -> Self {
		match err {
			HelperError::Internal(i) => Self::CommonError(CommonError::InternalError(i)),
			HelperError::BadRequest(b) => Self::CommonError(CommonError::BadRequest(b)),
			HelperError::InvalidBucketName(_) => Self::CommonError(CommonError::InvalidBucketName),
			HelperError::NoSuchBucket(_) => Self::CommonError(CommonError::NoSuchBucket),
			e => Self::CommonError(CommonError::BadRequest(format!("{}", e))),
		}
	}
}

impl From<SignatureError> for Error {
	fn from(err: SignatureError) -> Self {
		match err {
			SignatureError::CommonError(c) => Self::CommonError(c),
			SignatureError::AuthorizationHeaderMalformed(c) => {
				Self::AuthorizationHeaderMalformed(c)
			}
			SignatureError::InvalidUtf8Str(i) => Self::InvalidUtf8Str(i),
			SignatureError::InvalidHeader(h) => Self::InvalidHeader(h),
		}
	}
}

impl ApiError for Error {
	/// Get the HTTP status code that best represents the meaning of the error for the client
	fn http_status_code(&self) -> StatusCode {
		match self {
			Error::CommonError(c) => c.http_status_code(),
			Error::NoSuchKey => StatusCode::NOT_FOUND,
			Error::NotAcceptable(_) => StatusCode::NOT_ACCEPTABLE,
			Error::AuthorizationHeaderMalformed(_)
			| Error::InvalidBase64(_)
			| Error::InvalidHeader(_)
			| Error::InvalidUtf8Str(_) => StatusCode::BAD_REQUEST,
		}
	}

	fn add_http_headers(&self, _header_map: &mut HeaderMap<HeaderValue>) {
		// nothing
	}

	fn http_body(&self, garage_region: &str, path: &str) -> Body {
		// TODO nice json error
		Body::from(format!(
			"ERROR: {}\n\ngarage region: {}\npath: {}",
			self, garage_region, path
		))
	}
}