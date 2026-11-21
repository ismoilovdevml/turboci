//! Decoders of the Docker Engine API's streamed responses, for
//! `tokio_util::codec::FramedRead`: newline-delimited JSON messages (image
//! pulls, container waits) and the 8-byte-header frames of container logs and
//! exec output. FramedRead reads the body only while the decoder needs more
//! bytes and its stream is polled, so a slow consumer slows the reads.

use bytes::{Buf, Bytes, BytesMut};
use serde::de::DeserializeOwned;
use std::marker::PhantomData;

use super::Error;

/// One piece of a container's output
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogOutput {
    StdErr {
        message: Bytes,
    },
    StdOut {
        message: Bytes,
    },
    StdIn {
        message: Bytes,
    },
    /// Output that came without a frame header (a TTY's raw stream)
    Console {
        message: Bytes,
    },
}

impl std::fmt::Display for LogOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (LogOutput::StdErr { message }
        | LogOutput::StdOut { message }
        | LogOutput::StdIn { message }
        | LogOutput::Console { message }) = self;
        write!(f, "{}", String::from_utf8_lossy(message))
    }
}

#[derive(Debug, Clone, Copy)]
enum LogState {
    Header,
    /// Stream (0 stdin, 1 stdout, 2 stderr) and length of the payload
    Payload(u8, usize),
}

/// Splits multiplexed container output into its frames: an 8-byte header
/// (stream number, 3 zero bytes, big-endian u32 length) and the payload.
/// Output whose next byte cannot start a header (above 2) is raw TTY
/// output: on a hijacked exec connection everything buffered is passed on at
/// once, in a logs response it is passed on line by line.
#[derive(Debug, Clone, Copy)]
pub struct LogDecoder {
    state: LogState,
    hijacked: bool,
}

impl LogDecoder {
    /// For the body of a logs response
    pub fn logs() -> Self {
        Self {
            state: LogState::Header,
            hijacked: false,
        }
    }

    /// For the connection an exec start hijacked
    pub fn hijacked() -> Self {
        Self {
            state: LogState::Header,
            hijacked: true,
        }
    }
}

impl tokio_util::codec::Decoder for LogDecoder {
    type Item = LogOutput;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<LogOutput>, std::io::Error> {
        loop {
            match self.state {
                LogState::Header => {
                    if !src.is_empty() && src[0] > 2 {
                        if self.hijacked {
                            return Ok(Some(LogOutput::Console {
                                message: src.split().freeze(),
                            }));
                        }
                        return Ok(src.iter().position(|b| *b == b'\n').map(|end| {
                            LogOutput::Console {
                                message: src.split_to(end + 1).freeze(),
                            }
                        }));
                    }
                    if src.len() < 8 {
                        return Ok(None);
                    }
                    let header = src.split_to(8);
                    let length =
                        u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
                    self.state = LogState::Payload(header[0], length);
                }
                LogState::Payload(stream, length) => {
                    if src.len() < length {
                        // Room for the rest of the frame, read in one go
                        src.reserve(length - src.len());
                        return Ok(None);
                    }
                    let message = src.split_to(length).freeze();
                    self.state = LogState::Header;
                    return Ok(Some(match stream {
                        0 => LogOutput::StdIn { message },
                        1 => LogOutput::StdOut { message },
                        _ => LogOutput::StdErr { message },
                    }));
                }
            }
        }
    }
}

/// Splits a body into JSON messages. dockerd writes one per line, but a
/// message is taken from wherever it ends: blank lines, `\r\n` and a
/// message spread over several lines (pretty-printed) are all fine, and
/// `null` is skipped. Every call returns the next whole message in the
/// buffer if there is one, and asks for more bytes only when there is not.
/// The last message may lack its newline.
#[derive(Debug)]
pub struct JsonLineDecoder<T> {
    message: PhantomData<fn() -> T>,
}

impl<T> Default for JsonLineDecoder<T> {
    fn default() -> Self {
        Self {
            message: PhantomData,
        }
    }
}

impl<T: DeserializeOwned> tokio_util::codec::Decoder for JsonLineDecoder<T> {
    type Item = T;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<T>, Error> {
        loop {
            // The first value in the buffer and where it ends; whitespace
            // before it (blank lines, a `\r`) is skipped
            let (next, end) = {
                let mut values = serde_json::Deserializer::from_slice(src).into_iter::<Option<T>>();
                let next = values.next();
                (next, values.byte_offset())
            };
            match next {
                // Nothing but whitespace is left
                None => {
                    src.clear();
                    return Ok(None);
                }
                Some(Ok(message)) => {
                    src.advance(end);
                    if let Some(message) = message {
                        return Ok(Some(message));
                    }
                }
                // Cut short: the rest of it has not arrived yet
                Some(Err(e)) if e.is_eof() => return Ok(None),
                Some(Err(e)) if e.is_data() => {
                    return Err(Error::JsonData {
                        message: e.to_string(),
                    })
                }
                Some(Err(e)) => return Err(Error::Json(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio_util::codec::FramedRead;

    /// `decoder`'s output for a body that arrives in `chunks`
    async fn decode_chunks<D>(decoder: D, chunks: &[&[u8]]) -> Vec<Result<D::Item, D::Error>>
    where
        D: tokio_util::codec::Decoder,
        D::Error: From<std::io::Error>,
    {
        let chunks: Vec<std::io::Result<Bytes>> = chunks
            .iter()
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        let reader = tokio_util::io::StreamReader::new(futures_util::stream::iter(chunks));
        FramedRead::new(reader, decoder).collect().await
    }

    fn frame(stream: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![stream, 0, 0, 0];
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        frame
    }

    fn outputs(results: Vec<std::io::Result<LogOutput>>) -> Vec<LogOutput> {
        results.into_iter().map(Result::unwrap).collect()
    }

    fn out(text: &str) -> LogOutput {
        LogOutput::StdOut {
            message: Bytes::copy_from_slice(text.as_bytes()),
        }
    }

    fn err(text: &str) -> LogOutput {
        LogOutput::StdErr {
            message: Bytes::copy_from_slice(text.as_bytes()),
        }
    }

    #[tokio::test]
    async fn frames_split_anywhere_across_chunks_are_put_back_together() {
        let mut body = frame(1, b"hello\n");
        body.extend(frame(2, b"oops\n"));
        body.extend(frame(1, b"bye"));
        let expected = vec![out("hello\n"), err("oops\n"), out("bye")];

        for size in 1..=body.len() {
            let chunks: Vec<&[u8]> = body.chunks(size).collect();
            for decoder in [LogDecoder::logs(), LogDecoder::hijacked()] {
                let decoded = outputs(decode_chunks(decoder, &chunks).await);
                assert_eq!(decoded, expected, "chunks of {}", size);
            }
        }
    }

    #[tokio::test]
    async fn zero_length_frames_and_stdin_are_passed_on() {
        let mut body = frame(1, b"");
        body.extend(frame(0, b"in"));
        body.extend(frame(2, b""));

        let decoded = outputs(decode_chunks(LogDecoder::logs(), &[&body]).await);

        assert_eq!(
            decoded,
            vec![
                out(""),
                LogOutput::StdIn {
                    message: Bytes::from_static(b"in")
                },
                err("")
            ]
        );
    }

    #[tokio::test]
    async fn large_frames_arrive_whole() {
        let payload = vec![b'x'; 1 << 20];
        let body = frame(1, &payload);
        let chunks: Vec<&[u8]> = body.chunks(16 * 1024).collect();

        let decoded = outputs(decode_chunks(LogDecoder::hijacked(), &chunks).await);

        assert_eq!(decoded.len(), 1);
        assert_eq!(
            decoded[0],
            LogOutput::StdOut {
                message: Bytes::from(payload)
            }
        );
    }

    #[tokio::test]
    async fn a_frame_cut_short_by_the_end_of_the_body_is_an_error() {
        let body = frame(1, b"hello");

        let decoded = decode_chunks(LogDecoder::logs(), &[&body[..9]]).await;

        assert_eq!(decoded.len(), 1);
        let error = decoded[0].as_ref().unwrap_err();
        assert_eq!(error.to_string(), "bytes remaining on stream");
    }

    #[tokio::test]
    async fn raw_output_goes_by_line_in_logs_and_as_read_on_exec_connections() {
        let chunks: &[&[u8]] = &[b"one\ntw", b"o\nthree\n"];

        let logs = outputs(decode_chunks(LogDecoder::logs(), chunks).await);
        let exec = outputs(decode_chunks(LogDecoder::hijacked(), chunks).await);

        let console = |text: &str| LogOutput::Console {
            message: Bytes::copy_from_slice(text.as_bytes()),
        };
        assert_eq!(
            logs,
            vec![console("one\n"), console("two\n"), console("three\n")]
        );
        assert_eq!(exec, vec![console("one\ntw"), console("o\nthree\n")]);
        assert_eq!(console("ü\n").to_string(), "ü\n");
    }

    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct Progress {
        status: Option<String>,
        error: Option<String>,
    }

    fn progress(status: &str) -> Progress {
        Progress {
            status: Some(status.to_string()),
            error: None,
        }
    }

    #[tokio::test]
    async fn json_lines_split_anywhere_across_chunks_are_put_back_together() {
        let body = b"{\"status\":\"Pulling fs layer\"}\r\n{\"status\":\"Downloading\"}\r\n\
                     {\"errorDetail\":{\"message\":\"denied\"},\"error\":\"denied\"}\r\n";
        let expected = vec![
            progress("Pulling fs layer"),
            progress("Downloading"),
            Progress {
                status: None,
                error: Some("denied".to_string()),
            },
        ];

        // Whole lines in one chunk or several, or a line per chunk
        for chunks in [
            vec![&body[..]],
            body.split_inclusive(|b| *b == b'\n').collect::<Vec<_>>(),
            body.chunks(7).collect(),
        ] {
            let decoded: Vec<Progress> = decode_chunks(JsonLineDecoder::default(), &chunks)
                .await
                .into_iter()
                .map(Result::unwrap)
                .collect();
            assert_eq!(decoded, expected);
        }
    }

    #[tokio::test]
    async fn the_last_json_message_may_lack_its_newline() {
        let decoded: Vec<Progress> = decode_chunks(
            JsonLineDecoder::default(),
            &[b"{\"status\":\"a\"}\n{\"status\":\"b\"}"],
        )
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();

        assert_eq!(decoded, vec![progress("a"), progress("b")]);
    }

    #[tokio::test]
    async fn a_newline_inside_a_json_message_joins_the_lines() {
        let decoded: Vec<Progress> = decode_chunks(
            JsonLineDecoder::default(),
            &[b"{\"status\":\n\"a\"}\n{\"status\":\"b\"}\n"],
        )
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();

        assert_eq!(decoded, vec![progress("a"), progress("b")]);
    }

    /// Every message `decode` gives for `body` without asking for more bytes
    fn decode_buffered(body: &[u8]) -> Vec<Progress> {
        use tokio_util::codec::Decoder;

        let mut decoder = JsonLineDecoder::<Progress>::default();
        let mut buffer = BytesMut::from(body);
        let mut messages = Vec::new();
        while let Some(message) = decoder.decode(&mut buffer).unwrap() {
            messages.push(message);
        }
        messages
    }

    #[test]
    fn whole_json_messages_in_the_buffer_never_wait_for_more_bytes() {
        // Blank lines, whitespace-only lines and \r\n before a message
        assert_eq!(
            decode_buffered(b"\n\r\n  \t\n{\"status\":\"a\"}\r\n\r\n{\"status\":\"b\"}\n"),
            vec![progress("a"), progress("b")]
        );
        // A message spread over lines, then another one in the same chunk
        assert_eq!(
            decode_buffered(b"{\n  \"status\": \"a\"\n}\n{\"status\":\"b\"}\n"),
            vec![progress("a"), progress("b")]
        );
        // `null` is skipped
        assert_eq!(
            decode_buffered(b"null\n{\"status\":\"a\"}\n"),
            vec![progress("a")]
        );
        // A message cut short waits for the rest
        assert_eq!(
            decode_buffered(b"{\"status\":\"a\"}\n{\"stat"),
            vec![progress("a")]
        );
    }

    #[tokio::test]
    async fn json_messages_arrive_while_the_body_is_still_open() {
        // dockerd keeps the body open after these (a pull still running)
        let body: &[u8] =
            b"{\"status\":\"a\"}\n\n\n{\n\"status\":\n\"b\"\n}\r\n{\"status\":\"c\"}\n";
        let chunks =
            futures_util::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(body))])
                .chain(futures_util::stream::pending());
        let reader = tokio_util::io::StreamReader::new(chunks);
        let mut messages = FramedRead::new(reader, JsonLineDecoder::<Progress>::default());

        let mut decoded = Vec::new();
        for _ in 0..3 {
            let next = tokio::time::timeout(std::time::Duration::from_secs(5), messages.next())
                .await
                .expect("a message in the buffer waited for more bytes");
            decoded.push(next.unwrap().unwrap());
        }

        assert_eq!(decoded, vec![progress("a"), progress("b"), progress("c")]);
    }

    #[tokio::test]
    async fn blank_and_multi_line_json_split_anywhere_across_chunks() {
        let body: &[u8] =
            b"\r\n{\"status\":\"a\"}\r\n\r\n{\n  \"status\": \"b\"\n}\n\n{\"status\":\"c\"}\n\n  \n";
        for size in 1..=body.len() {
            let chunks: Vec<&[u8]> = body.chunks(size).collect();
            let decoded: Vec<Progress> = decode_chunks(JsonLineDecoder::default(), &chunks)
                .await
                .into_iter()
                .map(Result::unwrap)
                .collect();
            assert_eq!(
                decoded,
                vec![progress("a"), progress("b"), progress("c")],
                "chunks of {}",
                size
            );
        }
    }

    #[tokio::test]
    async fn the_last_json_message_may_lack_its_newline_after_blank_lines() {
        let decoded: Vec<Progress> = decode_chunks(
            JsonLineDecoder::default(),
            &[b"{\"status\":\"a\"}\n\n", b"\r\n{\"status\":", b"\"b\"}"],
        )
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();

        assert_eq!(decoded, vec![progress("a"), progress("b")]);
    }

    #[tokio::test]
    async fn a_json_message_of_the_wrong_shape_is_an_error() {
        let decoded = decode_chunks(
            JsonLineDecoder::<Progress>::default(),
            &[b"{\"status\":1}\n{\"status\":\"b\"}\n"],
        )
        .await;

        assert_eq!(decoded.len(), 1, "decoding stops at the error");
        let error = decoded[0].as_ref().unwrap_err().to_string();
        assert!(
            error.starts_with("Failed to deserialize JSON: invalid type: integer `1`"),
            "{}",
            error
        );
    }

    #[tokio::test]
    async fn a_json_message_cut_short_by_the_end_of_the_body_is_an_error() {
        let decoded = decode_chunks(
            JsonLineDecoder::<Progress>::default(),
            &[b"{\"status\":\"a\"}\n{\"st"],
        )
        .await;

        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].as_ref().unwrap(), &progress("a"));
        let error = decoded[1].as_ref().unwrap_err().to_string();
        assert_eq!(error, "bytes remaining on stream");
    }
}
