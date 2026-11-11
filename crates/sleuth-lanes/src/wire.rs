//! The lines that travel between the worker and a reader process.
//!
//! One JSON value per line. Worker to reader: first the settings, then
//! `{"id":N,"job":...}` per job. Reader to worker: `{"ready":true}` once the
//! settings are read, then `{"id":N,"reply":...}` per job, or
//! `{"id":N,"error":"..."}` when the job or its answer could not be put into
//! JSON. serde_json never writes a raw line break inside a value, so a line
//! is always one whole message.

use serde::{Deserialize, Serialize};
use std::io::BufRead;

/// A reading of a large drawing set is tens of MB. This limit only stops a
/// reader gone wrong from filling the worker's memory.
pub(crate) const MAX_LINE: usize = 1 << 30;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ready {
    pub ready: bool,
}

#[derive(Serialize)]
pub(crate) struct JobOut<'a, J> {
    pub id: u64,
    pub job: &'a J,
}

#[derive(Deserialize)]
pub(crate) struct JobIn<J> {
    pub id: u64,
    pub job: J,
}

#[derive(Serialize)]
pub(crate) struct ReplyOut<'a, R> {
    pub id: u64,
    pub reply: &'a R,
}

#[derive(Deserialize)]
pub(crate) struct ReplyIn<R> {
    pub id: u64,
    pub reply: R,
}

/// The reader is alive but has no answer for this job. Unknown fields are an
/// error so that a normal reply is told apart at its second key, without
/// walking through tens of MB first.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Refusal {
    pub id: u64,
    pub error: String,
}

/// Enough of a job line to answer it when the job itself cannot be read.
#[derive(Deserialize)]
pub(crate) struct IdOnly {
    pub id: u64,
}

/// Read one line into `line`, without its line break. `Ok(false)` at the end
/// of the stream; a last line cut off before its line break (the writer died
/// in the middle of it) counts as the end too.
pub(crate) fn read_line(r: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<bool> {
    line.clear();
    loop {
        let chunk = match r.fill_buf() {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if chunk.is_empty() {
            return Ok(false);
        }
        if let Some(end) = chunk.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&chunk[..end]);
            r.consume(end + 1);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(true);
        }
        let n = chunk.len();
        line.extend_from_slice(chunk);
        r.consume(n);
        if line.len() > MAX_LINE {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "a line longer than 1 GB"));
        }
    }
}

/// One message as a line, ready to write in a single call.
pub(crate) fn to_line(msg: &impl Serialize) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(msg)?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub(crate) fn is_blank(line: &[u8]) -> bool {
    line.iter().all(|b| b.is_ascii_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn lines_longer_than_the_buffer_come_back_whole() {
        let big = "x".repeat(300_000);
        let text = format!("first\n{big}\r\n\nlast\ncut off");
        let mut r = BufReader::with_capacity(4096, text.as_bytes());
        let mut line = Vec::new();
        let mut got = Vec::new();
        while read_line(&mut r, &mut line).unwrap() {
            got.push(String::from_utf8(line.clone()).unwrap());
        }
        assert_eq!(got, ["first", big.as_str(), "", "last"], "the unfinished last line is dropped");
    }

    #[test]
    fn a_reply_and_a_refusal_are_told_apart() {
        let reply = to_line(&ReplyOut { id: 7, reply: &Some("text\nwith a line break") }).unwrap();
        assert_eq!(reply.iter().filter(|b| **b == b'\n').count(), 1, "one line");
        assert!(serde_json::from_slice::<Refusal>(&reply).is_err());
        let back: ReplyIn<Option<String>> = serde_json::from_slice(&reply).unwrap();
        assert_eq!((back.id, back.reply.as_deref()), (7, Some("text\nwith a line break")));
        let refusal = to_line(&Refusal { id: 8, error: "no".into() }).unwrap();
        assert_eq!(serde_json::from_slice::<Refusal>(&refusal).unwrap().id, 8);
        assert!(serde_json::from_slice::<Ready>(&reply).is_err());
        assert!(serde_json::from_slice::<Ready>(b"{\"ready\":true}").unwrap().ready);
    }
}
