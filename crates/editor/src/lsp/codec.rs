//! LSP base protocol framing: `Content-Length` headers followed by a JSON body.

use std::io::{self, BufRead, Write};

use serde_json::Value;

pub fn write_message(writer: &mut dyn Write, value: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Reads one message. `Ok(None)` means the stream ended cleanly.
pub fn read_message(reader: &mut dyn BufRead) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let header = line.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            if content_length.is_some() {
                break;
            }
            // Tolerate stray blank lines between messages.
            continue;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(value.trim().parse().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "bad Content-Length")
                })?);
            }
        }
    }
    let length = content_length.expect("checked above");
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_messages() {
        let mut buffer = Vec::new();
        write_message(&mut buffer, &json!({"id": 1, "method": "initialize"})).unwrap();
        write_message(&mut buffer, &json!({"method": "initialized", "params": {"é": "ü"}})).unwrap();
        let mut reader = io::Cursor::new(buffer);
        assert_eq!(read_message(&mut reader).unwrap().unwrap()["id"], 1);
        assert_eq!(read_message(&mut reader).unwrap().unwrap()["params"]["é"], "ü");
        assert!(read_message(&mut reader).unwrap().is_none());
    }

    #[test]
    fn accepts_extra_headers_and_case() {
        let body = br#"{"jsonrpc":"2.0","id":7,"result":null}"#;
        let raw = format!(
            "content-length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n",
            body.len()
        );
        let mut data = raw.into_bytes();
        data.extend_from_slice(body);
        let value = read_message(&mut io::Cursor::new(data)).unwrap().unwrap();
        assert_eq!(value["id"], 7);
    }

    #[test]
    fn rejects_garbage_lengths() {
        let mut reader = io::Cursor::new(b"Content-Length: lots\r\n\r\n{}".to_vec());
        assert!(read_message(&mut reader).is_err());
    }
}
