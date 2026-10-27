//! Scripted [`MockProvider`] for tests.
//!
//! Available under `#[cfg(test)]` automatically, or for downstream crates
//! by enabling the `testing` feature on `kage-provider`.

use std::sync::{Arc, Mutex};

use kage_core::{CancelFlag, Content, Message, Role, sync::lock};

use crate::{EventStream, Provider, ProviderError, ProviderEvent, ProviderMetadata, StreamRequest};

/// Provider implementation that returns a fixed sequence of events.
///
/// Two construction modes:
/// - [`MockProvider::replaying`]: every call to [`Provider::stream`] returns
///   a fresh iterator over the same event list. Use this for "the model
///   always says X" scenarios.
/// - [`MockProvider::sequence`]: each call consumes one entry from a queue
///   of scripts. Use this for multi-turn replay where each turn yields
///   different events.
///
/// Requests are recorded so tests can assert what the loop sent.
#[derive(Clone, Debug)]
pub struct MockProvider {
    metadata: ProviderMetadata,
    requests: Arc<Mutex<Vec<StreamRequest>>>,
    mode: Arc<Mutex<ReplayMode>>,
}

#[derive(Debug)]
enum ReplayMode {
    Constant(Vec<Result<ProviderEvent, ProviderError>>),
    Sequence(Vec<Vec<Result<ProviderEvent, ProviderError>>>),
}

impl MockProvider {
    /// Construct a mock that returns the same script on every call.
    #[must_use]
    pub fn replaying(events: Vec<Result<ProviderEvent, ProviderError>>) -> Self {
        Self::with_mode(ReplayMode::Constant(events))
    }

    /// Construct a mock with a per-call queue of scripts.
    ///
    /// Once the queue is empty, further calls yield an empty iterator.
    #[must_use]
    pub fn sequence(scripts: Vec<Vec<Result<ProviderEvent, ProviderError>>>) -> Self {
        Self::with_mode(ReplayMode::Sequence(scripts))
    }

    fn with_mode(mode: ReplayMode) -> Self {
        Self {
            metadata: ProviderMetadata {
                id: "mock".into(),
                display_name: "Mock".into(),
                supports_caching: false,
                supports_thinking: true,
                supports_tool_use: true,
            },
            requests: Arc::new(Mutex::new(Vec::new())),
            mode: Arc::new(Mutex::new(mode)),
        }
    }

    /// Number of times [`Provider::stream`] has been invoked.
    #[must_use]
    pub fn call_count(&self) -> usize {
        lock(&self.requests).len()
    }

    /// Snapshot of all requests received, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<StreamRequest> {
        lock(&self.requests).clone()
    }

    /// Most recent request, if any.
    #[must_use]
    pub fn last_request(&self) -> Option<StreamRequest> {
        lock(&self.requests).last().cloned()
    }

    /// Append one script to the sequence queue, for scripts that need
    /// runtime values such as session ids from an earlier turn.
    pub fn push_script(&self, script: Vec<Result<ProviderEvent, ProviderError>>) {
        if let ReplayMode::Sequence(scripts) = &mut *lock(&self.mode) {
            scripts.push(script);
        }
    }
}

impl Provider for MockProvider {
    fn metadata(&self) -> &ProviderMetadata {
        &self.metadata
    }

    fn stream(
        &self,
        req: StreamRequest,
        _cancel: &CancelFlag,
    ) -> Result<EventStream, ProviderError> {
        lock(&self.requests).push(req);
        let events: Vec<Result<ProviderEvent, ProviderError>> = {
            let mut mode = lock(&self.mode);
            match &mut *mode {
                ReplayMode::Constant(events) => events.clone(),
                ReplayMode::Sequence(scripts) => {
                    if scripts.is_empty() {
                        Vec::new()
                    } else {
                        scripts.remove(0)
                    }
                }
            }
        };
        Ok(Box::new(events.into_iter()))
    }
}

/// A user message holding `text`.
#[must_use]
pub fn user_msg(text: &str) -> Message {
    Message::new(
        Role::User,
        vec![Content::Text {
            text: text.to_owned(),
        }],
        None,
    )
}

/// Collect every event of `stream`, panicking on the first error.
pub fn collect_ok(
    stream: impl Iterator<Item = Result<ProviderEvent, ProviderError>>,
) -> Vec<ProviderEvent> {
    stream.map(|r| r.expect("stream item is Ok")).collect()
}

/// Read one HTTP request from `conn`: the head, then as many body
/// bytes as its `content-length` names. Mock servers call this before
/// answering, because Windows resets a connection that closes with
/// unread data and the client then loses the answer.
#[cfg(test)]
pub(crate) fn read_request(conn: &mut std::net::TcpStream) -> String {
    String::from_utf8(read_request_bytes(conn)).unwrap()
}

/// [`read_request`] as raw bytes, for a multipart body, which is not
/// UTF-8.
#[cfg(test)]
pub(crate) fn read_request_bytes(conn: &mut std::net::TcpStream) -> Vec<u8> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = std::io::Read::read(conn, &mut buf).unwrap();
        raw.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&raw).to_lowercase();
    let length: usize = head
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .map_or(0, |v| v.trim().parse().unwrap());
    let body_start = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    while raw.len() < body_start + length {
        let n = std::io::Read::read(conn, &mut buf).unwrap();
        raw.extend_from_slice(&buf[..n]);
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StopReason;
    use kage_core::TokenUsage;

    fn end_event() -> ProviderEvent {
        ProviderEvent::MessageEnd {
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
        }
    }

    #[test]
    fn replaying_returns_same_events_each_call() {
        let mock = MockProvider::replaying(vec![Ok(ProviderEvent::MessageStart), Ok(end_event())]);
        for _ in 0..3 {
            let stream = mock
                .stream(StreamRequest::new("m", vec![]), &CancelFlag::new())
                .unwrap();
            let events: Vec<_> = stream.collect();
            assert_eq!(events.len(), 2);
        }
        assert_eq!(mock.call_count(), 3);
    }

    #[test]
    fn sequence_consumes_scripts_in_order() {
        let mock = MockProvider::sequence(vec![
            vec![Ok(ProviderEvent::TextDelta { delta: "a".into() })],
            vec![Ok(ProviderEvent::TextDelta { delta: "b".into() })],
        ]);
        let first: Vec<_> = mock
            .stream(StreamRequest::new("m", vec![]), &CancelFlag::new())
            .unwrap()
            .collect();
        let second: Vec<_> = mock
            .stream(StreamRequest::new("m", vec![]), &CancelFlag::new())
            .unwrap()
            .collect();
        let third: Vec<_> = mock
            .stream(StreamRequest::new("m", vec![]), &CancelFlag::new())
            .unwrap()
            .collect();
        assert!(
            matches!(first[0].as_ref().unwrap(), ProviderEvent::TextDelta { delta } if delta == "a")
        );
        assert!(
            matches!(second[0].as_ref().unwrap(), ProviderEvent::TextDelta { delta } if delta == "b")
        );
        assert!(third.is_empty());
    }

    #[test]
    fn requests_are_recorded() {
        let mock = MockProvider::replaying(vec![]);
        let _ = mock.stream(StreamRequest::new("m1", vec![]), &CancelFlag::new());
        let _ = mock.stream(StreamRequest::new("m2", vec![]), &CancelFlag::new());
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].model, "m1");
        assert_eq!(reqs[1].model, "m2");
        assert_eq!(mock.last_request().unwrap().model, "m2");
    }

    #[test]
    fn errors_in_script_propagate() {
        let mock = MockProvider::replaying(vec![Err(ProviderError::Auth("nope".into()))]);
        let stream = mock
            .stream(StreamRequest::new("m", vec![]), &CancelFlag::new())
            .unwrap();
        let events: Vec<_> = stream.collect();
        assert!(matches!(events[0], Err(ProviderError::Auth(_))));
    }
}
