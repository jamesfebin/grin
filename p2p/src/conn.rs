// Copyright 2018 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Provides a connection wrapper that handles the lower level tasks in sending
//! or receiving data from the TCP socket, as well as dealing with timeouts.
//!
//! Because of a few idiosyncracies in the Rust `TcpStream`, this has to use
//! async I/O to be able to both read *and* write on the connection. Which
//! forces us to go through some additional gymnastic to loop over the async
//! stream and make sure we get the right number of bytes out.

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::net::TcpStream;
use std::sync::{mpsc, Arc};
use std::{cmp, thread, time};

use core::ser;
use msg::{read_body, read_exact, read_header, write_all, write_to_buf, MsgHeader, Type};
use types::Error;
use util::{RateCounter, RwLock};

/// A trait to be implemented in order to receive messages from the
/// connection. Allows providing an optional response.
pub trait MessageHandler: Send + 'static {
	fn consume<'a>(&self, msg: Message<'a>) -> Result<Option<Response<'a>>, Error>;
}

// Macro to simplify the boilerplate around async I/O error handling,
// especially with WouldBlock kind of errors.
macro_rules! try_break {
	($chan:ident, $inner:expr) => {
		match $inner {
			Ok(v) => Some(v),
			Err(Error::Connection(ref e)) if e.kind() == io::ErrorKind::WouldBlock => None,
			Err(e) => {
				let _ = $chan.send(e);
				break;
				}
			}
	};
}

/// A message as received by the connection. Provides access to the message
/// header lazily consumes the message body, handling its deserialization.
pub struct Message<'a> {
	pub header: MsgHeader,
	conn: &'a mut TcpStream,
}

impl<'a> Message<'a> {
	fn from_header(header: MsgHeader, conn: &'a mut TcpStream) -> Message<'a> {
		Message { header, conn }
	}

	/// Get the TcpStream
	pub fn get_conn(&mut self) -> TcpStream {
		return self.conn.try_clone().unwrap();
	}

	/// Read the message body from the underlying connection
	pub fn body<T>(&mut self) -> Result<T, Error>
	where
		T: ser::Readable,
	{
		read_body(&self.header, self.conn)
	}

	pub fn copy_attachment(&mut self, len: usize, writer: &mut Write) -> Result<usize, Error> {
		let mut written = 0;
		while written < len {
			let read_len = cmp::min(8000, len - written);
			let mut buf = vec![0u8; read_len];
			read_exact(
				&mut self.conn,
				&mut buf[..],
				time::Duration::from_secs(10),
				true,
			)?;
			writer.write_all(&mut buf)?;
			written += read_len;
		}
		Ok(written)
	}

	/// Respond to the message with the provided message type and body
	pub fn respond<T>(self, resp_type: Type, body: T) -> Response<'a>
	where
		T: ser::Writeable,
	{
		let body = ser::ser_vec(&body).unwrap();
		Response {
			resp_type: resp_type,
			body: body,
			conn: self.conn,
			attachment: None,
		}
	}
}

/// Response to a `Message`
pub struct Response<'a> {
	resp_type: Type,
	body: Vec<u8>,
	conn: &'a mut TcpStream,
	attachment: Option<File>,
}

impl<'a> Response<'a> {
	fn write(mut self) -> Result<(), Error> {
		let mut msg =
			ser::ser_vec(&MsgHeader::new(self.resp_type, self.body.len() as u64)).unwrap();
		msg.append(&mut self.body);
		write_all(&mut self.conn, &msg[..], time::Duration::from_secs(10))?;
		if let Some(mut file) = self.attachment {
			let mut buf = [0u8; 8000];
			loop {
				match file.read(&mut buf[..]) {
					Ok(0) => break,
					Ok(n) => write_all(&mut self.conn, &buf[..n], time::Duration::from_secs(10))?,
					Err(e) => return Err(From::from(e)),
				}
			}
		}
		Ok(())
	}

	pub fn add_attachment(&mut self, file: File) {
		self.attachment = Some(file);
	}
}

pub const SEND_CHANNEL_CAP: usize = 10;

pub struct Tracker {
	/// Bytes we've sent.
	pub sent_bytes: Arc<RwLock<RateCounter>>,
	/// Bytes we've received.
	pub received_bytes: Arc<RwLock<RateCounter>>,
	/// Channel to allow sending data through the connection
	pub send_channel: mpsc::SyncSender<Vec<u8>>,
	/// Channel to close the connection
	pub close_channel: mpsc::Sender<()>,
	/// Channel to check for errors on the connection
	pub error_channel: mpsc::Receiver<Error>,
}

impl Tracker {
	pub fn send<T>(&self, body: T, msg_type: Type) -> Result<(), Error>
	where
		T: ser::Writeable,
	{
		let buf = write_to_buf(body, msg_type);
		let buf_len = buf.len();
		self.send_channel.try_send(buf)?;

		// Increase sent bytes counter
		let mut sent_bytes = self.sent_bytes.write();
		sent_bytes.inc(buf_len as u64);

		Ok(())
	}
}

/// Start listening on the provided connection and wraps it. Does not hang
/// the current thread, instead just returns a future and the Connection
/// itself.
pub fn listen<H>(stream: TcpStream, handler: H) -> Tracker
where
	H: MessageHandler,
{
	let (send_tx, send_rx) = mpsc::sync_channel(SEND_CHANNEL_CAP);
	let (close_tx, close_rx) = mpsc::channel();
	let (error_tx, error_rx) = mpsc::channel();

	// Counter of number of bytes received
	let received_bytes = Arc::new(RwLock::new(RateCounter::new()));

	stream
		.set_nonblocking(true)
		.expect("Non-blocking IO not available.");
	poll(
		stream,
		handler,
		send_rx,
		error_tx,
		close_rx,
		received_bytes.clone(),
	);

	Tracker {
		sent_bytes: Arc::new(RwLock::new(RateCounter::new())),
		received_bytes: received_bytes.clone(),
		send_channel: send_tx,
		close_channel: close_tx,
		error_channel: error_rx,
	}
}

fn poll<H>(
	conn: TcpStream,
	handler: H,
	send_rx: mpsc::Receiver<Vec<u8>>,
	error_tx: mpsc::Sender<Error>,
	close_rx: mpsc::Receiver<()>,
	received_bytes: Arc<RwLock<RateCounter>>,
) where
	H: MessageHandler,
{
	let mut conn = conn;
	let _ = thread::Builder::new()
		.name("peer".to_string())
		.spawn(move || {
			let sleep_time = time::Duration::from_millis(1);

			let conn = &mut conn;
			let mut retry_send = Err(());
			loop {
				// check the read end
				if let Some(h) = try_break!(error_tx, read_header(conn, None)) {
					let msg = Message::from_header(h, conn);
					trace!(
						"Received message header, type {:?}, len {}.",
						msg.header.msg_type,
						msg.header.msg_len
					);

					// Increase received bytes counter
					{
						let mut received_bytes = received_bytes.write();
						received_bytes.inc(size_of::<MsgHeader>() as u64 + msg.header.msg_len);
					}

					if let Some(Some(resp)) = try_break!(error_tx, handler.consume(msg)) {
						try_break!(error_tx, resp.write());
					}
				}

				// check the write end, use or_else so try_recv is lazily eval'd
				let maybe_data = retry_send.or_else(|_| send_rx.try_recv());
				retry_send = Err(());
				if let Ok(data) = maybe_data {
					let written =
						try_break!(error_tx, conn.write_all(&data[..]).map_err(&From::from));
					if written.is_none() {
						retry_send = Ok(data);
					}
				}

				// check the close channel
				if let Ok(_) = close_rx.try_recv() {
					debug!(
						"Connection close with {} initiated by us",
						conn.peer_addr()
							.map(|a| a.to_string())
							.unwrap_or("?".to_owned())
					);
					break;
				}

				thread::sleep(sleep_time);
			}
		});
}
