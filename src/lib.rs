extern crate hyper;
extern crate multipart;
extern crate url;
#[macro_use]
extern crate mime;
extern crate rustc_serialize;
extern crate time;
extern crate crypto;
extern crate rusqlite;
extern crate tempdir;
extern crate rand;

mod rest;
mod error;

pub use error::{Result, Error};

use url::{Url, form_urlencoded};
use std::process::Command;
use std::io::{self, Read, Write};
use rustc_serialize::{json, Decodable, Encodable};
use std::fs::{self, File};
use time::Timespec;
use std::path::{Path, Component};
use rest::RestBuilder;
use std::time::Duration;
use hyper::status::StatusCode;
use crypto::md5::Md5;
use crypto::digest::Digest;
use hyper::http;
use hyper::client::pool::Pool;
use std::path::PathBuf;
use std::str;
use rand::Rng;
use std::cmp;


/// How many times we retry contacting Amazon after a server error
const MAXIMUM_RETRY: u32 = 5;
/// How many hours to hold onto an endpoint (after which the endpoint is refreshed)
const REFRESH_ENDPOINT_TIME: i64 = 3*24;


pub struct Client {
	config_dir: PathBuf,
	security_profile: SecurityProfile,
	authorization: Authorization,
	endpoint: Endpoint,
	root_id: NodeId,
	cache_connection: rusqlite::Connection,
	protocol: Box<http::Protocol>,
}

#[derive(Clone, PartialEq, Debug)]
pub struct NodeId(String);

#[derive(RustcEncodable, RustcDecodable)]
struct SecurityProfile {
	pub client_id: String,
	pub client_secret: String,
}

#[derive(RustcEncodable, RustcDecodable)]
struct Authorization {
	pub access_token: String,
	pub refresh_token: String,
	pub token_type: String,
	pub date_last_updated: i64,
}

#[derive(RustcEncodable, RustcDecodable)]
struct Endpoint {
	pub content_url: String,
	pub metadata_url: String,
	pub date_last_updated: i64,
}

#[derive(RustcDecodable, Debug)]
struct O2TokenResponse {
	pub access_token: String,
	pub refresh_token: String,
	pub token_type: String,
	pub expires_in: u64,
}

#[derive(RustcDecodable, Debug)]
struct NodeResponse {
	pub id: String,
}

#[derive(RustcDecodable, Debug)]
#[allow(non_snake_case)]
struct NodesResponse {
	pub count: u64,
	pub nextToken: Option<String>,
	pub data: Vec<NodeResponse>,
}


impl Client {
	/// Create a new instances of AmazonCloudDrive.
	/// client_id and client_secret come from an Amazon security profile.  A security profile is not,
	/// like one would expect, the login crednetials for a Cloud Drive account.  Rather they
	/// are the credentials belonging to an Amazon App.  They're generated by a developer (anyone
	/// can sign up as an Amzon developer).
	/// When we authorize we are asking Amazon to allow our "app" to access the user's Cloud Drive.
	/// Amazon will have the user log in and ask them to authorize our "app".
	/// It's ... weird and bothersome, but that's the only way to use the API, so.
	/// To get a client_id and client_secret, you'll need to create an Amazon developer account,
	/// generate a security profile, and get that security profile approved by Amazon.
	///
	/// When creating a new instance without any pre-existing configuration, the user will be
	/// prompted to give access to their Amazon Cloud Drive account.  The authorization will be
	/// saved to the config_dir so it can be re-used in the future and not prompt the user again.
	pub fn new<P: AsRef<Path>>(client_id: &str, client_secret: &str, config_dir: P) -> Result<Client> {
		let config_dir = config_dir.as_ref().join(".acd");

		// Create configuration directory
		try!(fs::create_dir_all(&config_dir));

		let cache_conn = try!(Client::init_cache(&config_dir));

		let security_profile = SecurityProfile {
			client_id: client_id.to_owned(),
			client_secret: client_secret.to_owned(),
		};

		// Read existing endpoint or start from scratch.
		let endpoint = read_json_file(config_dir.join("endpoint.json")).unwrap_or(Endpoint {
			content_url: String::new(),
			metadata_url: String::new(),
			date_last_updated: 0,
		});

		// Read existing authorization or start from scratch.
		let authorization = read_json_file(config_dir.join("authorization.json")).unwrap_or(Authorization {
			access_token: String::new(),
			refresh_token: String::new(),
			token_type: String::new(),
			date_last_updated: 0,
		});

		let mut acd = Client {
			config_dir: config_dir,
			security_profile: security_profile,
			authorization: authorization,
			endpoint: endpoint,
			root_id: NodeId(String::new()),
			cache_connection: cache_conn,
			protocol: Box::new(http::h1::Http11Protocol::with_connector(Pool::new(Default::default()))),
		};

		// If we aren't authorized yet, authorize.
		if acd.authorization.access_token.is_empty() {
			try!(acd.authorize());
		}

		// TODO: Technically we need to call refresh_endpoint before every REST call that uses
		// the endpoint, so that if the library is run for a long time (3+ days) it'll follow spec.
		// Right now this should be good enough, and it's unlikely that the endpoint will change anyway.
		try!(acd.refresh_endpoint());
		acd.root_id = try!(acd.find_root());

		Ok(acd)
	}

	fn init_cache<P: AsRef<Path>>(config_dir: P) -> Result<rusqlite::Connection> {
		let conn = try!(rusqlite::Connection::open(config_dir.as_ref().join("cache.sqlite")));

		// Set up tables if they don't exist
		try!(conn.execute("CREATE TABLE IF NOT EXISTS path_cache (
			parent TEXT NOT NULL,
			name TEXT NOT NULL,
			id TEXT NOT NULL
		)", &[]));
		try!(conn.execute("CREATE INDEX IF NOT EXISTS idx_path_cache_parent_name ON path_cache (parent, name);", &[]));
		try!(conn.execute("CREATE INDEX IF NOT EXISTS idx_path_cache_parent ON path_cache (parent);", &[]));

		Ok(conn)
	}

	fn insert_into_node_cache(&mut self, &NodeId(ref parent): &NodeId, name: &str, id: &str) -> Result<()> {
		try!(self.cache_connection.execute("INSERT INTO path_cache (parent, name, id) VALUES (?,?,?)", &[&parent.to_owned(), &name.to_owned(), &id.to_owned()]));
		Ok(())
	}

	fn fetch_from_node_cache(&self, &NodeId(ref parent): &NodeId, name: &str) -> Result<Option<NodeId>> {
		let result = self.cache_connection.query_row("SELECT id FROM path_cache WHERE parent=? AND name=?", &[&parent.to_owned(), &name.to_owned()], |row| {
        	NodeId(row.get(0))
    	});

		match result {
			Ok(id) => Ok(Some(id)),
			Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
			Err(err) => Err(Error::from(err)),
		}
	}

	// Make the request to the server and get the response.
	// If there's a server error, try again using the recommended backoff method.
	// If our access token has expired, we will attempt renew it.
	fn get_server_response_with_retry(&mut self, rest: RestBuilder, authorize: bool) -> Result<(StatusCode, Vec<u8>)> {
		let mut retry_count = 0;

		loop {
			let rest_copy = rest.clone();
			let rest_copy = if authorize {
				rest_copy.authorization(&(self.authorization.access_token.clone()))
			} else {
				rest_copy
			};

			// Backoff
			if retry_count > 0 {
				let backoff = rand::thread_rng().gen_range(0, 1000 * (1 << (cmp::min(retry_count - 1, 8))));
				std::thread::sleep(Duration::from_millis(backoff));
			}

			let (status_code, body) = match self.get_server_response(rest_copy) {
				Ok((status_code, body)) => (status_code, body),
				Err(Error::ExpiredToken) => if authorize {
					// Need reauthentication
					try!(self.refresh_authorization());
					retry_count = 0;  // Successful authorization means we got a successful response from the server, so reset the retry_count.
					continue;
				} else {
					// Server told us our access token was expired, but we didn't provide one...
					return Err(Error::ServerError(format!("Server reported Expired Token on a call that didn't have a token.")));
				},
				Err(err) => {
					// Communication error, retry
					retry_count += 1;
					if retry_count >= MAXIMUM_RETRY {
						return Err(err);
					}
					println!("INFO: Communication Error, will retry: {:?}", err);
					continue;
				}
			};

			// Server errors will cause us to retry.  All other errors just return.
			if status_code.class() == hyper::status::StatusClass::ServerError {
				retry_count += 1;
				if retry_count >= MAXIMUM_RETRY {
					return Err(Error::ServerError(format!("Status was {}, Body was {:?}", status_code, String::from_utf8(body))));
				}
				println!("INFO: Server Error, will retry: Status Code: {:?}", status_code);
				continue;
			}

			return Ok((status_code, body));
		}
	}

	// Make the request to the server and get the response.
	fn get_server_response(&mut self, rest: RestBuilder) -> Result<(StatusCode, Vec<u8>)> {
		#[derive(RustcDecodable, Debug)]
		struct MessageResponse {
			message: String,
		}

		let mut response = try!(rest.send(&self.protocol));

		let mut body = vec![0u8; 0];
		try!(response.read_to_end(&mut body));

		if response.status.is_success() {
			return Ok((response.status, body));
		}

		// Errors usually have some JSON error message associated with them
		let body_json: Option<MessageResponse> = match str::from_utf8(&body) {
			Ok(s) => match json::decode(&s) {
				Ok(msg) => Some(msg),
				Err(_) => None,
			},
			Err(_) => None,
		};

		// The ACD API is supposed to return 401 when we need to reauth, but I found them returning
		// 400 Bad Request, with a JSON message saying the status code was 401 and that the token had expired.
		// ...Whut?
		// So don't analyze status code; just check for "Token has expired"
		if let Some(msg) = body_json {
			if msg.message.contains("Token has expired") {
				return Err(Error::ExpiredToken)
			}
		}

		Ok((response.status, body))
	}

	fn refresh_endpoint(&mut self) -> Result<()> {
		#[derive(RustcDecodable, Debug)]
		#[allow(non_snake_case)]
		struct AccountEndpointResponse {
			contentUrl: String,
			metadataUrl: String,
		}

		let date_last_updated = Timespec::new(self.endpoint.date_last_updated, 0);
		let now = time::get_time();

		if (now - date_last_updated).num_hours() < REFRESH_ENDPOINT_TIME {
			return Ok(())
		}

		let request = RestBuilder::get("https://drive.amazonaws.com/drive/v1/account/endpoint");
		let (status_code, body) = try!(self.get_server_response_with_retry(request, true));

		let response: AccountEndpointResponse = match status_code {
			StatusCode::Ok => {
				try!(decode_server_json(&body))
			},
			_ => return Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
		};

		self.endpoint = Endpoint {
			content_url: response.contentUrl,
			metadata_url: response.metadataUrl,
			date_last_updated: time::get_time().sec,
		};

		try!(write_json_file(self.config_dir.join("endpoint.json"), &self.endpoint));

		Ok(())
	}

	fn authorize(&mut self) -> Result<()> {
		/* First, direct the user to the Amazon login page */
		open_webbrowser(&("https://www.amazon.com/ap/oa?".to_string() + &form_urlencoded::serialize(&[
			("client_id", &self.security_profile.client_id),
			("scope", &"clouddrive:read_all clouddrive:write".to_owned()),
			("response_type", &"code".to_owned()),
			("redirect_uri", &"http://localhost:26619/".to_owned())
		])));

		/* After they login, their browser will redirect to the authorization URL which contains the
		 * code we need.  The user should copy the URL from their browser and paste it into the console
		 */
		println!("Paste the response url:");
		let code = {
			let mut response_url = String::new();
			try!(io::stdin().read_line(&mut response_url));

			let response_pairs = try!(try!(Url::parse(&response_url)).query_pairs().ok_or(Error::BadAuthUrl));
			let code = try!(response_pairs.iter().find(|&x| x.0 == "code").ok_or(Error::BadAuthUrl));
			&code.1.clone()
		};

		/* Get authorization tokens from Amazon using the code */
		let request = RestBuilder::post("https://api.amazon.com/auth/o2/token")
			.body_query(&[
				("grant_type", "authorization_code"),
				("code", code),
				("client_id", &self.security_profile.client_id),
				("client_secret", &self.security_profile.client_secret),
				("redirect_uri", "http://localhost:26619/")
			]);
		let (status_code, body) = try!(self.get_server_response_with_retry(request, false));

		let response: O2TokenResponse = match status_code {
				StatusCode::Ok => {
					try!(decode_server_json(&body))
				},
				_ => return Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
			};

		self.authorization = Authorization {
			access_token: response.access_token,
			refresh_token: response.refresh_token,
			token_type: response.token_type,
			date_last_updated: time::get_time().sec,
		};

		try!(write_json_file(self.config_dir.join("authorization.json"), &self.authorization));

		Ok(())
	}

	fn refresh_authorization(&mut self) -> Result<()> {
		println!("Refreshing authorization");

		let request = RestBuilder::post("https://api.amazon.com/auth/o2/token")
			.body_query(&[
				("grant_type", "refresh_token"),
				("refresh_token", &self.authorization.refresh_token),
				("client_id", &self.security_profile.client_id),
				("client_secret", &self.security_profile.client_secret),
				("redirect_uri", "http://localhost:26619/")
			]);
		let (status_code, body) = try!(self.get_server_response_with_retry(request, false));

		let response: O2TokenResponse = match status_code {
			StatusCode::Ok => {
				try!(decode_server_json(&body))
			},
			_ => return Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
		};

		self.authorization = Authorization {
			access_token: response.access_token,
			refresh_token: response.refresh_token,
			token_type: response.token_type,
			date_last_updated: time::get_time().sec,
		};

		try!(write_json_file(self.config_dir.join("authorization.json"), &self.authorization));

		Ok(())
	}

	fn find_root(&mut self) -> Result<NodeId> {
		let request = RestBuilder::get(&self.endpoint.metadata_url.clone())
			.url_push("nodes")
			.url_query(&[("filters", "kind:FOLDER AND isRoot:true")]);

		let (status_code, body) = try!(self.get_server_response_with_retry(request, true));

		match status_code {
			StatusCode::Ok => {
				let response: NodesResponse = try!(decode_server_json(&body));
				Ok(NodeId(response.data[0].id.clone()))
			},
			_ => Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
		}
	}

	pub fn find_child(&mut self, parent: &NodeId, name: &str) -> Result<Option<NodeId>> {
		if let Some(id) = try!(self.fetch_from_node_cache(parent, name)) {
			return Ok(Some(id));
		}

		let request = RestBuilder::get(&self.endpoint.metadata_url)
			.url_push("nodes")
			.url_push(&parent.0)
			.url_push("children")
			.url_query(&[("filters", "name:".to_owned() + name)]);
		let (status_code, body) = try!(self.get_server_response_with_retry(request, true));

		match status_code {
			StatusCode::Ok => {
				let response: NodesResponse = try!(decode_server_json(&body));
				if response.data.len() == 0 {
					return Ok(None);
				}
				try!(self.insert_into_node_cache(parent, name, &response.data[0].id));
				Ok(Some(NodeId(response.data[0].id.clone())))
			},
			_ => return Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
		}
	}

	/// Find a node using an absolute or relative path.
	/// Returns None if the path could not be found.
	pub fn find_path<P: AsRef<Path>>(&mut self, parent: Option<&NodeId>, path: P) -> Result<Option<NodeId>> {
		let mut current_dir = parent.unwrap_or(&self.root_id).clone();

		for p in path.as_ref().components() {
			match p {
				Component::RootDir => current_dir = self.root_id.clone(),
				Component::CurDir => (),
				Component::Normal(osstr) => match osstr.to_str() {
					Some(name) => current_dir = match try!(self.find_child(&current_dir, name)) {
						Some(child) => child,
						None => return Ok(None),
					},
					None => return Err(Error::BadPath),
				},
				_ => return Err(Error::BadPath),
			}
		}

		Ok(Some(current_dir))
	}

	/// Upload `data` to ACD with filename `name` under parent `parent`.  The NodeId for the new file
	/// is returned.  If we return successfully, the file is guaranteed to have been uploaded without
	/// corruption, at least within the guarantees provided by Amazon Cloud Drive.
	pub fn upload(&mut self, parent: Option<&NodeId>, name: &str, data: &[u8], content_type: Option<mime::Mime>) -> Result<NodeId> {
		#[derive(RustcEncodable)]
		struct UploadMetadata {
			name: String,
			kind: String,
			parents: Vec<String>,
		}

		#[derive(RustcDecodable, Debug)]
		struct NodeUploadResponseContentProperties {
			md5: String,
		}

		#[derive(RustcDecodable, Debug)]
		#[allow(non_snake_case)]
		struct NodeUploadResponse {
			id: String,
			contentProperties: NodeUploadResponseContentProperties,
		}

		let calculated_md5 = {
			let mut md5 = Md5::new();
			md5.input(data);
			md5.result_str().to_lowercase()
		};

		let parent = parent.unwrap_or(&self.root_id).clone();

		let metadata = UploadMetadata {
			name: name.to_owned(),
			kind: "FILE".to_owned(),
			parents: vec![parent.0.clone()],
		};

		let content_type = content_type.unwrap_or("application/octect-stream".parse().unwrap());

		let request = RestBuilder::post(&self.endpoint.content_url)
			.url_push("nodes")
			.url_query(&[("suppress", "deduplication")])
			.multipart_data("metadata", try!(json::encode(&metadata)).as_bytes(), None, None)
			.multipart_data("content", data, Some(name.to_owned()), Some(content_type));

		let (status_code, body) = try!(self.get_server_response_with_retry(request, true));

		match status_code {
			StatusCode::Created => {
				let response: NodeUploadResponse = try!(decode_server_json(&body));

				if response.contentProperties.md5.to_lowercase() != calculated_md5 {
					panic!("UH OH!!!! During an upload Amazon returned a bad MD5. This is very bad. We don't handle this case. Oh dear...");
					// TODO: Handle this by deleting the file and returning an error
				}

				try!(self.insert_into_node_cache(&parent, name, &response.id));

				Ok(NodeId(response.id))
			},
			StatusCode::Conflict => Err(Error::NodeExists),
			_ => Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
		}
	}

	/// Create directory if it doesn't exist.
	/// Returns id for created/existing directory.
	/// If parent is None then parent will be the root node.
	pub fn mkdir(&mut self, parent: Option<&NodeId>, name: &str) -> Result<NodeId> {
		#[derive(RustcEncodable)]
		struct Metadata {
			name: String,
			kind: String,
			parents: Vec<String>,
		}

		#[derive(RustcDecodable)]
		struct Response {
			id: String,
		}

		#[derive(RustcDecodable)]
		#[allow(non_snake_case)]
		struct ConflictResponseInfo {
			nodeId: String,
		}

		#[derive(RustcDecodable)]
		struct ConflictResponse {
			info: ConflictResponseInfo,
		}

		let parent = parent.unwrap_or(&self.root_id).clone();

		if let Some(id) = try!(self.fetch_from_node_cache(&parent, name)) {
			return Ok(id);
		}

		let metadata = Metadata {
			name: name.to_owned(),
			kind: "FOLDER".to_owned(),
			parents: vec![parent.0.clone()],
		};

		let request = RestBuilder::post(&self.endpoint.metadata_url)
			.url_push("nodes")
			.body(try!(json::encode(&metadata)).as_bytes());

		let (status_code, body) = try!(self.get_server_response_with_retry(request, true));

		match status_code {
			StatusCode::Created => {
				let response: Response = try!(decode_server_json(&body));
				try!(self.insert_into_node_cache(&parent, name, &response.id));
				Ok(NodeId(response.id))
			},
			StatusCode::Conflict => {
				let response: ConflictResponse = try!(decode_server_json(&body));
				try!(self.insert_into_node_cache(&parent, name, &response.info.nodeId));
				Ok(NodeId(response.info.nodeId))
			},
			_ => Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
		}
	}

	/// Create all directories in path if they don't exist
	/// Returns id for the last directory in the path
	pub fn mkdir_all<P: AsRef<Path>>(&mut self, parent: Option<&NodeId>, path: P) -> Result<NodeId> {
		let mut current_dir = parent.unwrap_or(&self.root_id).clone();

		for p in path.as_ref().components() {
			match p {
				Component::RootDir => current_dir = self.root_id.clone(),
				Component::CurDir => (),
				Component::Normal(osstr) => {
					let name = try!(osstr.to_str().ok_or(Error::BadPath));
					current_dir = try!(self.mkdir(Some(&current_dir), name));
				},
				_ => return Err(Error::BadPath),
			}
		}

		Ok(current_dir)
	}

	pub fn ls(&mut self, parent: &NodeId) -> Result<Vec<NodeId>> {
		let mut ids = Vec::new();
		let mut next_token = None;

		loop {
			let request = RestBuilder::get(&self.endpoint.metadata_url)
				.url_push("nodes")
				.url_push(&parent.0)
				.url_push("children");
			let request = match next_token {
				Some(token) => request.url_query(&[("startToken", token)]),
				None => request,
			};
			let (status_code, body) = try!(self.get_server_response_with_retry(request, true));

			let response: NodesResponse = match status_code {
				StatusCode::Ok => {
					try!(decode_server_json(&body))
				},
				_ => return Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
			};

			for node in response.data {
				ids.push(NodeId(node.id.clone()))
			}

			match response.nextToken {
				Some(token) => next_token = Some(token),
				None => break,
			}
		}

		Ok(ids)
	}

	pub fn download(&mut self, id: &NodeId) -> Result<Vec<u8>> {
		let request = RestBuilder::get(&self.endpoint.content_url)
			.url_push("nodes").url_push(&id.0).url_push("content");
		let (status_code, body) = try!(self.get_server_response_with_retry(request, true));

		match status_code {
			StatusCode::Ok => Ok(body),
			_ => return Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
		}
	}

	/// Delete a node.
	/// NOTE: This only sends the node to the Trash.  The user needs to manually empty their trash.
	pub fn rm(&mut self, node: &NodeId) -> Result<()> {
		let request = RestBuilder::put(&self.endpoint.metadata_url)
			.url_push("trash")
			.url_push(&node.0);

		let (status_code, body) = try!(self.get_server_response_with_retry(request, true));

		match status_code {
			StatusCode::Ok => Ok(()),
			_ => Err(Error::UnknownServerError(format!("Unknown Server Response, probably an error. Status was {}, Body was {:?}", status_code, String::from_utf8(body)))),
		}
	}
}


fn read_json_file<T: Decodable, P: AsRef<Path>>(path: P) -> Result<T> {
	let mut f = try!(File::open(path));
	let mut s = String::new();
	try!(f.read_to_string(&mut s));
	Ok(try!(json::decode(&s)))
}


fn write_json_file<T: Encodable, P: AsRef<Path>>(path: P, value: &T) -> Result<()> {
	let mut f = try!(File::create(path));
	try!(f.write_all (&try!(json::encode(value)).into_bytes()));
	Ok(())
}


fn decode_server_json<T: Decodable>(s: &[u8]) -> Result<T> {
	match String::from_utf8(s.to_vec()) {
		Ok(s) => {
			json::decode(&s).map_err(|e| Error::ResponseBadJson(e))
		},
		Err(_) => {
			Err(Error::ResponseNotUtf8(s.to_vec()))
		},
	}
}


fn open_webbrowser(url: &str) {
	Command::new("xdg-open").arg(url).output().unwrap();
}


#[cfg(test)]
mod test {
	use super::{Client, read_json_file, SecurityProfile};
	use super::Error as AcdError;
	use tempdir::TempDir;
	use std::path::Path;
	use rand::{self, Rng};

	// TODO: Test concurrent instances to make sure they don't stomp eachother's config_dir.
	#[test]
	fn test_everything() {
		let security_profile: SecurityProfile = read_json_file("test.security_profile.json").unwrap();
		let temp_config_dir = TempDir::new("rust-acd-test").unwrap();
		let temp_upload_dir = temp_config_dir.path().file_name().unwrap();
		let mut client = Client::new(&security_profile.client_id, &security_profile.client_secret, temp_config_dir.path()).unwrap();
		println!("temp_upload_dir: {:?}", temp_upload_dir);

		// Test mkdir_all
		let mkdir_test_dir = client.mkdir_all(None, Path::new("/").join(temp_upload_dir).join("mkdir_test")).unwrap();
		let temp_upload_dir = client.find_path(None, Path::new("/").join(temp_upload_dir)).unwrap().unwrap();

		// Test upload
		let small_data: Vec<u8> = rand::thread_rng().gen_iter().take(4).collect();
		let large_data: Vec<u8> = rand::thread_rng().gen_iter().take(1024*1024).collect();
		let small_data_node = client.upload(Some(&mkdir_test_dir), "small_data", &small_data, None).unwrap();
		let large_data_node = client.upload(Some(&mkdir_test_dir), "large_data", &large_data, None).unwrap();

		// Test find_path
		assert_eq!(client.find_path(Some(&temp_upload_dir), Path::new("mkdir_test").join("small_data")).unwrap().unwrap(), small_data_node);

		// Test download
		assert_eq!(client.download(&small_data_node).unwrap(), small_data);
		assert_eq!(client.download(&large_data_node).unwrap(), large_data);

		// Test conflict
		match client.upload(Some(&mkdir_test_dir), "small_data", b"if you see this text, something is broken", None) {
			Err(AcdError::NodeExists) => (),
			_ => panic!("upload should throw an error if we try to specify a filename that already exists."),
		}

		// Test handling missing files
		assert!(client.find_path(Some(&temp_upload_dir), Path::new("thisdoesntexist")).unwrap().is_none());

		// Test ls
		let ls_result = client.ls(&mkdir_test_dir).unwrap();
		assert_eq!(ls_result.len(), 2);
		assert!((ls_result[0] == small_data_node && ls_result[1] == large_data_node) || (ls_result[0] == large_data_node && ls_result[1] == small_data_node));

		// Cleanup
		client.rm(&temp_upload_dir).unwrap();
	}
}
