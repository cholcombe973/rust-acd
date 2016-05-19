use std::error::Error as StdError;
use hyper::error::Error as HyperError;
use std::io::Error as IoError;
use rustc_serialize::json::DecoderError as JsonError;
use rusqlite::Error as SqliteError;
use rustc_serialize::json::EncoderError as JsonEncoderError;
use rustc_serialize::json::DecoderError as JsonDecoderError;
use url::ParseError as UrlParseError;
use std::fmt;


use self::Error::*;

pub type Result<T> = ::std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
	/// Error from Hyper (HTTP client)
	Hyper(HyperError),
	/// I/O error
	Io(IoError),
	/// Error from rusqlite
	Sqlite(SqliteError),
	/// Error from the JSON encoder
	JsonEncoder(JsonEncoderError),
	/// Error from the JSON decoder
	JsonDecoder(JsonDecoderError),
	/// Url Parse Error
	UrlParse(UrlParseError),
	/// Need a new access token
	ExpiredToken,
	/// Bad Authentication URL
	BadAuthUrl,
	/// Invalid path.  The path specified could not be parsed.
	BadPath,
	/// Server response was expected to be a string, but we couldn't decode it as UTF-8
	ResponseNotUtf8(Vec<u8>),
	/// Server response was supposed to be JSON, but we couldn't decode as expected
	ResponseBadJson(JsonError),
	/// Server's response was not as expected, probably an error
	UnknownServerError(String),
	/// The server returned a 5xx status code
	ServerError(String),
	/// Node (file/directory) exists
	NodeExists,
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		f.write_str(self.description())
	}
}

impl StdError for Error {
	fn description(&self) -> &str {
		match *self {
			Hyper(ref e) => e.description(),
			Io(ref e) => e.description(),
			Sqlite(ref e) => e.description(),
			JsonEncoder(ref e) => e.description(),
			JsonDecoder(ref e) => e.description(),
			UrlParse(ref e) => e.description(),
			ExpiredToken => "Access Token Expired",
			BadPath => "Invalid path provided",
			BadAuthUrl => "Invalid authorization URL provided",
			ResponseNotUtf8(_) => "Server response was supposed to be UTF-8, but wasn't",
			ResponseBadJson(ref e) => e.description(),
			UnknownServerError(ref e) => e,
			ServerError(ref e) => e,
			NodeExists => "Node exists",
		}
	}

	fn cause(&self) -> Option<&StdError> {
		match *self {
			Hyper(ref error) => Some(error),
			Io(ref error) => Some(error),
			Sqlite(ref error) => Some(error),
			JsonEncoder(ref error) => Some(error),
			JsonDecoder(ref error) => Some(error),
			UrlParse(ref error) => Some(error),
			ExpiredToken => None,
			BadPath => None,
			BadAuthUrl => None,
			ResponseNotUtf8(_) => None,
			ResponseBadJson(ref error) => Some(error),
			UnknownServerError(_) => None,
			ServerError(_) => None,
			NodeExists => None,
		}
	}
}

impl From<HyperError> for Error {
	fn from(err: HyperError) -> Error {
		Hyper(err)
	}
}

impl From<IoError> for Error {
	fn from(err: IoError) -> Error {
		Io(err)
	}
}

impl From<SqliteError> for Error {
	fn from(err: SqliteError) -> Error {
		Sqlite(err)
	}
}

impl From<JsonEncoderError> for Error {
	fn from(err: JsonEncoderError) -> Error {
		JsonEncoder(err)
	}
}

impl From<JsonDecoderError> for Error {
	fn from(err: JsonDecoderError) -> Error {
		JsonDecoder(err)
	}
}

impl From<UrlParseError> for Error {
	fn from(err: UrlParseError) -> Error {
		UrlParse(err)
	}
}
