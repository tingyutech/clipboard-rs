use bytes::{BufMut, Bytes, BytesMut};

use clipboard_rs::common::RustImage;
use clipboard_rs::{
	Clipboard, ClipboardContext, ClipboardHandler, ClipboardWatcher, ClipboardWatcherContext,
	FilePasteHandler, WatcherShutdown,
};
use flate2::write::GzEncoder;
use flate2::Compression;
use xxhash_rust::xxh32::xxh32;

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tokio::{
	io::AsyncWriteExt,
	select,
	sync::{
		mpsc::{channel, error::TrySendError, Receiver, Sender},
		Mutex as TMutex,
	},
	task::spawn_blocking,
	time::sleep,
};

use crate::file_list::FileListItem;

const HASHER_SEED: u32 = 0x333333;

pub struct Clippy {
	receiver: TMutex<Receiver<Bytes>>,
	clipboard_manager: ClipboardManager,
}

#[derive(Clone)]
struct ClipboardManager {
	sender: Sender<Bytes>,
	ctx: Option<ClipboardContext>,
	last_hash: u32,
	file_lists: HashMap<u32, Vec<FileListItem>>,
}

impl ClipboardManager {
	pub fn new(sender: Sender<Bytes>) -> Self {
		let ctx = ClipboardContext::new_with_paste_handler(FilePasteHandlerImpl).ok();
		Self {
			sender,
			last_hash: 0,
			ctx,
			file_lists: HashMap::new(),
		}
	}

	fn enqueue(&self, payload: Bytes) {
		if let Err(err) = self.sender.try_send(payload) {
			match err {
				TrySendError::Full(_) => {
					println!("Channel is full");
				}
				TrySendError::Closed(_) => {
					println!("Channel is closed");
				}
			}
		}
	}

	fn valid_payload(&mut self, payload: &[u8]) -> bool {
		let hash = xxh32(payload, HASHER_SEED);
		if hash == self.last_hash {
			return false;
		}
		self.last_hash = hash;
		true
	}
}

impl ClipboardHandler for ClipboardManager {
	fn on_clipboard_change(&mut self) {
		println!("change");
		let ctx = if let Some(ctx) = self.ctx.as_ref() {
			ctx
		} else {
			println!("No context on clipboard change");
			return;
		};

		if let Ok(image) = ctx.get_image() {
			println!("got image");
			let png = match image.to_png() {
				Ok(img) => img,
				Err(err) => {
					println!("Copied image can't convert to png: {}", err);
					return;
				}
			};
			let png_bytes = png.get_bytes();
			if !self.valid_payload(png_bytes) {
				return;
			}
			println!("sync local clipboard image to remote");
			let mut bytes = BytesMut::with_capacity(png_bytes.len() + 1);
			bytes.put_u8(1);
			bytes.put_slice(png_bytes);
			self.enqueue(bytes.freeze());

			return;
		}
		if let Ok(file_list) = ctx.get_files() {
			if !file_list.is_empty() {
				println!("got filelist");
				let file_list_bytes = file_list.join("").into_bytes();
				if !self.valid_payload(&file_list_bytes) {
					return;
				}

				let file_list = file_list
					.into_iter()
					.map(FileListItem::new)
					.collect::<anyhow::Result<Vec<_>>>();
				let file_list = match file_list {
					Err(err) => {
						println!("failed to generate file_list: {}", err);
						return;
					}
					Ok(file_list) => file_list,
				};

				let file_list_json = serde_json::to_string_pretty(&file_list).unwrap();
				let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
				encoder.write_all(file_list_json.as_bytes()).unwrap();
				let compressed = encoder.finish().unwrap();

				self.file_lists
					.insert(xxh32(&file_list_bytes, HASHER_SEED), file_list);
				let mut bytes = BytesMut::with_capacity(compressed.len());
				// bytes.put_u8(2);
				bytes.put_slice(&compressed);
				self.enqueue(bytes.freeze());

				return;
			}
		}
		if let Ok(text) = ctx.get_text() {
			if !self.valid_payload(text.as_bytes()) {
				return;
			}
			println!("got text:{}", text.len());
			let mut bytes = BytesMut::with_capacity(text.len() + 1);
			bytes.put_u8(0);
			bytes.put_slice(text.as_bytes());
			self.enqueue(bytes.freeze());

			return;
		}

		println!("unreachable");
	}
}

impl Clippy {
	pub fn new() -> Self {
		let (sender, receiver) = channel::<Bytes>(5);
		Self {
			receiver: TMutex::new(receiver),
			clipboard_manager: ClipboardManager::new(sender),
		}
	}

	pub async fn wait_message(&self) {
		loop {
			let in_msg = r#"[
            {
                "Uri": "file:///home/me/Videos/screen-record",
                "Path": "/home/me/Videos/screen-record",
                "Size": 4096,
                "Attributes": 16,
                "CreationTime": "2024-06-12T11:36:49Z",
                "LastAccessTime": "2024-06-25T08:54:50Z",
                "LastWriteTime": "2024-06-21T09:56:41Z"
            },
            {
                "Uri": "file:///home/me/Documents/example.txt",
                "Path": "/home/mw/Documents/example.txt",
                "Size": 14,
                "Attributes": 128,
                "CreationTime": "2024-06-26T08:02:05Z",
                "LastAccessTime": "2024-06-26T08:02:05Z",
                "LastWriteTime": "2024-06-26T08:02:02Z"
            }
            ] "#;
			println!("Got filelist!");
			let file_list = serde_json::from_str::<Vec<FileListItem>>(in_msg).unwrap();
			let uris: Vec<_> = file_list.iter().map(|item| item.uri.to_string()).collect();
			self.clipboard_manager
				.ctx
				.as_ref()
				.unwrap()
				.set_files(uris)
				.unwrap();

			sleep(Duration::from_secs(20000000)).await;
		}
	}

	pub fn start_watch(&self) -> WatcherShutdown {
		let ctx = self.clipboard_manager.ctx.clone().unwrap();
		let mut watcher = ClipboardWatcherContext::new(ctx);
		let watcher_shutdown = watcher
			.add_handler(self.clipboard_manager.clone())
			.get_shutdown_channel();
		spawn_blocking(move || {
			println!("start");
			watcher.start_watch();
			println!("end");
		});
		watcher_shutdown
	}

	pub async fn handle_send(&self) -> anyhow::Result<()> {
		let mut receiver = self.receiver.lock().await;
		loop {
			if let Some(payload) = receiver.recv().await {
				println!("sync clipboard");
				let mut writer = Vec::new();
				// writer.write_u8(65).await?;
				// writer.write_u32_le(payload.len() as u32).await?;
				AsyncWriteExt::write_all(&mut writer, &payload).await?;
				AsyncWriteExt::flush(&mut writer).await?;

				// let mut decode_result = String::new();
				// let mut decoder = GzDecoder::new(writer.as_slice());
				// decoder.read_to_string(&mut decode_result).unwrap();
				// println!("{decode_result}");

				println!("sync finished.")
			}
		}
	}

	pub async fn run(&self) -> anyhow::Result<()> {
		select! {
			_ = self.handle_send() => {

			}
			_ = self.wait_message() => {

			}
		};

		Ok(())
	}
}

impl Default for Clippy {
	fn default() -> Self {
		Self::new()
	}
}

struct FilePasteHandlerImpl;

impl FilePasteHandler for FilePasteHandlerImpl {
	fn on_file_list_paste(&self, file_list: &[String]) -> clipboard_rs::Result<Vec<String>> {
		println!("Catch paste request! original filelist: {file_list:?}\n Modifying file_list...");
		let file_path = "/tmp/example.txt".to_string();
		let exists = Path::new(&file_path).try_exists()?;

		if !exists {
			let mut file = std::fs::File::create(&file_path)?;
			writeln!(&mut file, "Hello, world!")?;
			file.sync_all()?;
		}
		let new = vec![file_path];
		println!("Replaced filelist with {new:?}");
		Ok(new)
	}
}

#[tokio::main]
async fn main() {
	let clippy = Clippy::new();
	let shutdown = clippy.start_watch();
	clippy.run().await.unwrap();
	shutdown.stop();
}

mod file_list {
	use std::{fs, os::unix::fs::MetadataExt, path::Path, time::UNIX_EPOCH};

	use anyhow::Context;
	use chrono::{DateTime, Utc};
	use serde::{Deserialize, Serialize};

	#[derive(Debug, Clone, Serialize, Deserialize)]
	#[serde(rename_all = "PascalCase")]
	pub struct FileListItem {
		pub uri: String,
		pub path: String,
		pub size: u64,
		pub attributes: u32,
		pub creation_time: DateTime<Utc>,
		pub last_access_time: DateTime<Utc>,
		pub last_write_time: DateTime<Utc>,
	}

	#[repr(u32)]
	pub enum Attribute {
		Directory = 0x10,
		Normal = 0x80,
	}

	impl From<Attribute> for u32 {
		fn from(value: Attribute) -> Self {
			value as u32
		}
	}

	impl FileListItem {
		pub fn new(uri: String) -> anyhow::Result<Self> {
			let path = uri.trim_start_matches("file://").to_string();
			let file_path = Path::new(&path);
			let metadata = fs::metadata(file_path)?;

			let (attributes, last_write_time, last_access_time) = if cfg!(windows) {
				// use std::{
				// 	ffi::{OsStr, OsString},
				// 	os::windows::ffi::OsStrExt,
				// };
				// use windows::core::PWSTR;
				// use windows::Win32::Foundation::{GetLastError, INVALID_HANDLE_VALUE};
				// use windows::Win32::Storage::FileSystem::GetFileAttributesW;
				// unsafe {
				// 	let mut windows_filename: Vec<u16> = OsStr::new(&path)
				// 		.encode_wide()
				// 		.chain(std::iter::once(0))
				// 		.collect();
				// 	let attributes = GetFileAttributesW(PWSTR(windows_filename.as_mut_ptr()));
				// 	if attributes == INVALID_HANDLE_VALUE.0 as u32 {
				// 		let error = GetLastError();
				// 		anyhow::bail!("encounter error when getting attributes: {}", error.0);
				// 	}
				// 	(attributes, todo!(), todo!())
				// }
				todo!()
			} else if cfg!(unix) {
				let attributes = if metadata.is_file() {
					Attribute::Normal as u32
				} else if metadata.is_dir() {
					Attribute::Directory as u32
				} else {
					anyhow::bail!("{} is neither a file nor a directory", path);
				};
				let last_write_time = DateTime::from_timestamp(metadata.mtime(), 0)
					.context("Error convert mtime to DateTime")?;
				let last_access_time = DateTime::from_timestamp(metadata.atime(), 0)
					.context("Error convert atime to DateTime")?;
				(attributes, last_write_time, last_access_time)
			} else {
				anyhow::bail!("Unsupported")
			};

			let size = metadata.len();

			let duration_since_epoch = metadata.created()?.duration_since(UNIX_EPOCH)?.as_secs();
			let creation_time = DateTime::from_timestamp(duration_since_epoch as i64, 0)
				.context("Error convert creation time to DataTime")?;

			Ok(Self {
				uri,
				path,
				size,
				attributes,
				creation_time,
				last_access_time,
				last_write_time,
			})
		}
	}
}
