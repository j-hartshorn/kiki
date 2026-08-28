#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use async_openai::Client;
    use async_openai::config::OpenAIConfig;
    use async_openai::error::OpenAIError;
    use async_openai::middleware::ReqwestService;
    use futures::{Stream, StreamExt};
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio::time::{sleep, timeout};

    const COMPLETED_STREAM: &str = concat!(
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"sequence_number\":1,\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"Checking\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"sequence_number\":2,\"item_id\":\"fc_1\",\"output_index\":1,\"delta\":\"{\\\"code\\\":\\\"print(1)\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"sequence_number\":3,\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"service_tier\":\"default\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"Checking\"}],\"encrypted_content\":\"encrypted-reasoning\",\"status\":\"completed\"},{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"execute_python\",\"arguments\":\"{\\\"code\\\":\\\"print(1)\\\"}\",\"status\":\"completed\"}],\"usage\":{\"input_tokens\":23,\"input_tokens_details\":{\"cached_tokens\":5,\"cache_write_tokens\":7},\"output_tokens\":11,\"output_tokens_details\":{\"reasoning_tokens\":3},\"total_tokens\":34}}}\n\n",
        "data: [DONE]\n\n",
    );
    const FAILED_STREAM: &str = concat!(
        "data: {\"type\":\"response.failed\",\"sequence_number\":1,\"response\":{\"id\":\"resp_failed\",\"status\":\"failed\",\"output\":[],\"error\":{\"code\":\"server_error\",\"message\":\"failed\"}}}\n\n",
        "data: [DONE]\n\n",
    );
    const INCOMPLETE_STREAM: &str = concat!(
        "data: {\"type\":\"response.incomplete\",\"sequence_number\":1,\"response\":{\"id\":\"resp_incomplete\",\"status\":\"incomplete\",\"output\":[],\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
        "data: [DONE]\n\n",
    );
    const ERROR_STREAM: &str = concat!(
        "data: {\"type\":\"error\",\"sequence_number\":1,\"code\":\"stream_error\",\"message\":\"stream failed\",\"param\":null}\n\n",
        "data: [DONE]\n\n",
    );
    const ONE_DELTA: &str = "data: {\"type\":\"response.reasoning_summary_text.delta\",\"sequence_number\":1,\"item_id\":\"rs_1\",\"output_index\":0,\"summary_index\":0,\"delta\":\"partial\"}\n\n";

    type WireStream = Pin<Box<dyn Stream<Item = Result<WireEvent, OpenAIError>> + Send>>;

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type")]
    enum WireEvent {
        #[serde(rename = "response.completed")]
        Completed {
            sequence_number: u64,
            response: WireResponse,
        },
        #[serde(rename = "response.failed")]
        Failed {
            sequence_number: u64,
            response: WireResponse,
        },
        #[serde(rename = "response.incomplete")]
        Incomplete {
            sequence_number: u64,
            response: WireResponse,
        },
        #[serde(rename = "response.reasoning_summary_text.delta")]
        ReasoningSummaryDelta { sequence_number: u64, delta: String },
        #[serde(rename = "response.function_call_arguments.delta")]
        FunctionArgumentsDelta { sequence_number: u64, delta: String },
        #[serde(rename = "error")]
        Error {
            sequence_number: u64,
            code: Option<String>,
            message: String,
        },
    }

    #[derive(Debug, Deserialize)]
    struct WireResponse {
        id: String,
        status: WireStatus,
        #[serde(default)]
        output: Vec<WireOutputItem>,
        service_tier: Option<String>,
        usage: Option<WireUsage>,
        error: Option<WireResponseError>,
        incomplete_details: Option<WireIncompleteDetails>,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(rename_all = "snake_case")]
    enum WireStatus {
        Completed,
        Failed,
        Incomplete,
    }

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    enum WireOutputItem {
        Reasoning {
            id: String,
            summary: Vec<Value>,
            encrypted_content: Option<String>,
            status: String,
        },
        FunctionCall {
            id: String,
            call_id: String,
            name: String,
            arguments: String,
            status: String,
        },
    }

    #[derive(Debug, Deserialize)]
    struct WireUsage {
        input_tokens: u64,
        input_tokens_details: WireInputTokenDetails,
        output_tokens: u64,
        output_tokens_details: WireOutputTokenDetails,
        total_tokens: u64,
    }

    #[derive(Debug, Deserialize)]
    struct WireInputTokenDetails {
        cached_tokens: u64,
        cache_write_tokens: u64,
    }

    #[derive(Debug, Deserialize)]
    struct WireOutputTokenDetails {
        reasoning_tokens: u64,
    }

    #[derive(Debug, Deserialize)]
    struct WireResponseError {
        code: String,
        message: String,
    }

    #[derive(Debug, Deserialize)]
    struct WireIncompleteDetails {
        reason: String,
    }

    #[derive(Clone)]
    enum FixtureReply {
        Sse(&'static str),
        Status { code: u16, body: &'static str },
        AbruptSse(&'static str),
        IdleSse(&'static str),
    }

    #[derive(Clone)]
    struct CapturedRequest {
        head: String,
        body: Value,
    }

    struct Fixture {
        endpoint: String,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
        disconnected: Arc<AtomicBool>,
        task: JoinHandle<()>,
    }

    impl Fixture {
        async fn start(reply: FixtureReply) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let disconnected = Arc::new(AtomicBool::new(false));
            let task = {
                let requests = requests.clone();
                let disconnected = disconnected.clone();
                tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let request = read_request(&mut stream).await;
                        requests.lock().unwrap().push(request);
                        write_reply(&mut stream, &reply, &disconnected).await;
                    }
                })
            };
            Self {
                endpoint,
                requests,
                disconnected,
                task,
            }
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn request(&self) -> CapturedRequest {
            self.requests.lock().unwrap()[0].clone()
        }

        async fn wait_for_disconnect(&self) {
            timeout(Duration::from_secs(1), async {
                while !self.disconnected.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("dropping the SDK stream should close the fixture connection");
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn read_request(stream: &mut TcpStream) -> CapturedRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "fixture connection closed before request headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break position + 4;
            }
        };
        let head = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let content_length = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() < header_end + content_length {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0, "fixture connection closed before request body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        let body = serde_json::from_slice(&bytes[header_end..header_end + content_length]).unwrap();
        CapturedRequest { head, body }
    }

    async fn write_reply(stream: &mut TcpStream, reply: &FixtureReply, disconnected: &AtomicBool) {
        match reply {
            FixtureReply::Sse(body) => {
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            FixtureReply::Status { code, body } => {
                let response = format!(
                    "HTTP/1.1 {code} Fixture Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            FixtureReply::AbruptSse(body) => {
                write_chunked_headers(stream).await;
                write_chunk(stream, body).await;
            }
            FixtureReply::IdleSse(body) => {
                write_chunked_headers(stream).await;
                write_chunk(stream, body).await;
                let mut byte = [0];
                if stream.read(&mut byte).await.unwrap_or(0) == 0 {
                    disconnected.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    async fn write_chunked_headers(stream: &mut TcpStream) {
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
            )
            .await
            .unwrap();
    }

    async fn write_chunk(stream: &mut TcpStream, body: &str) {
        stream
            .write_all(format!("{:X}\r\n{}\r\n", body.len(), body).as_bytes())
            .await
            .unwrap();
    }

    fn sdk_client(endpoint: &str) -> Client<OpenAIConfig> {
        Client::with_config(
            OpenAIConfig::new()
                .with_api_key("fixture-key")
                .with_api_base(endpoint),
        )
        .with_http_service(ReqwestService::default())
    }

    fn execute_python_request() -> Value {
        json!({
            "model": "gpt-5.6-sol",
            "input": "compatibility fixture",
            "stream": true,
            "store": false,
            "service_tier": "default",
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "reasoning": {
                "effort": "medium",
                "summary": "auto",
                "context": "all_turns"
            },
            "include": ["reasoning.encrypted_content"],
            "tools": [{
                "type": "function",
                "name": "execute_python",
                "strict": true,
                "parameters": {
                    "type": "object",
                    "properties": {
                        "code": {"type": "string"}
                    },
                    "required": ["code"],
                    "additionalProperties": false
                }
            }]
        })
    }

    async fn open_stream_with_request(
        fixture: &Fixture,
        request: Value,
    ) -> Result<WireStream, OpenAIError> {
        sdk_client(&fixture.endpoint)
            .responses()
            .create_stream_byot(request)
            .await
    }

    async fn open_stream(fixture: &Fixture) -> Result<WireStream, OpenAIError> {
        open_stream_with_request(fixture, execute_python_request()).await
    }

    async fn next_event(stream: &mut WireStream) -> Result<WireEvent, OpenAIError> {
        timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("fixture should produce a stream event")
            .expect("fixture stream should remain open")
    }

    #[tokio::test]
    async fn sdk_compatibility_sends_exact_request_and_decodes_completed_data() {
        let fixture = Fixture::start(FixtureReply::Sse(COMPLETED_STREAM)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        match next_event(&mut stream).await.unwrap() {
            WireEvent::ReasoningSummaryDelta {
                sequence_number,
                delta,
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(delta, "Checking");
            }
            event => panic!("unexpected first event: {event:?}"),
        }
        match next_event(&mut stream).await.unwrap() {
            WireEvent::FunctionArgumentsDelta {
                sequence_number,
                delta,
            } => {
                assert_eq!(sequence_number, 2);
                assert_eq!(delta, r#"{"code":"print(1)"}"#);
            }
            event => panic!("unexpected second event: {event:?}"),
        }
        let response = match next_event(&mut stream).await.unwrap() {
            WireEvent::Completed {
                sequence_number,
                response,
            } => {
                assert_eq!(sequence_number, 3);
                response
            }
            event => panic!("unexpected terminal event: {event:?}"),
        };

        assert_eq!(response.id, "resp_1");
        assert_eq!(response.status, WireStatus::Completed);
        assert_eq!(response.service_tier.as_deref(), Some("default"));
        match &response.output[0] {
            WireOutputItem::Reasoning {
                encrypted_content, ..
            } => {
                assert_eq!(encrypted_content.as_deref(), Some("encrypted-reasoning"));
            }
            output => panic!("unexpected first output: {output:?}"),
        }
        match &response.output[1] {
            WireOutputItem::FunctionCall {
                name, arguments, ..
            } => {
                assert_eq!(name, "execute_python");
                assert_eq!(arguments, r#"{"code":"print(1)"}"#);
            }
            output => panic!("unexpected second output: {output:?}"),
        }
        let usage = response.usage.unwrap();
        assert_eq!(usage.input_tokens, 23);
        assert_eq!(usage.input_tokens_details.cached_tokens, 5);
        assert_eq!(usage.input_tokens_details.cache_write_tokens, 7);
        assert_eq!(usage.output_tokens, 11);
        assert_eq!(usage.output_tokens_details.reasoning_tokens, 3);
        assert_eq!(usage.total_tokens, 34);
        assert!(stream.next().await.is_none());

        let captured = fixture.request();
        assert!(captured.head.starts_with("POST /responses HTTP/1.1\r\n"));
        assert_eq!(
            captured.body,
            json!({
                "model": "gpt-5.6-sol",
                "input": "compatibility fixture",
                "stream": true,
                "store": false,
                "service_tier": "default",
                "tool_choice": "auto",
                "parallel_tool_calls": false,
                "reasoning": {
                    "effort": "medium",
                    "summary": "auto",
                    "context": "all_turns"
                },
                "include": ["reasoning.encrypted_content"],
                "tools": [{
                    "type": "function",
                    "name": "execute_python",
                    "strict": true,
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "code": {"type": "string"}
                        },
                        "required": ["code"],
                        "additionalProperties": false
                    }
                }]
            })
        );
    }

    #[tokio::test]
    async fn sdk_compatibility_replays_completed_output_items_before_function_output() {
        let fixture = Fixture::start(FixtureReply::Sse(COMPLETED_STREAM)).await;
        let mut first_stream = open_stream(&fixture).await.unwrap();
        let first_response = loop {
            match next_event(&mut first_stream).await.unwrap() {
                WireEvent::Completed { response, .. } => break response,
                _ => {}
            }
        };
        assert_eq!(first_response.output.len(), 2);
        assert!(first_stream.next().await.is_none());

        let call_id = match &first_response.output[1] {
            WireOutputItem::FunctionCall { call_id, .. } => call_id.clone(),
            output => panic!("unexpected function call output: {output:?}"),
        };
        let mut second_input = first_response
            .output
            .iter()
            .map(|output| serde_json::to_value(output).unwrap())
            .collect::<Vec<_>>();
        second_input.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": "1\n"
        }));
        let mut second_request = execute_python_request();
        second_request["input"] = Value::Array(second_input);

        let mut second_stream = open_stream_with_request(&fixture, second_request)
            .await
            .unwrap();
        while let Some(event) = second_stream.next().await {
            event.unwrap();
        }

        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|request| request.body["store"] == json!(false))
        );
        assert_eq!(
            requests[1].body["input"],
            json!([
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "summary": [{"type": "summary_text", "text": "Checking"}],
                    "encrypted_content": "encrypted-reasoning",
                    "status": "completed"
                },
                {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "execute_python",
                    "arguments": r#"{"code":"print(1)"}"#,
                    "status": "completed"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "1\n"
                }
            ])
        );
    }

    #[tokio::test]
    async fn sdk_compatibility_decodes_failed_event_and_ends_stream() {
        let fixture = Fixture::start(FixtureReply::Sse(FAILED_STREAM)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        match next_event(&mut stream).await.unwrap() {
            WireEvent::Failed {
                sequence_number,
                response,
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(response.id, "resp_failed");
                assert_eq!(response.status, WireStatus::Failed);
                let error = response.error.unwrap();
                assert_eq!(error.code, "server_error");
                assert_eq!(error.message, "failed");
            }
            event => panic!("unexpected failed event: {event:?}"),
        }
        assert!(stream.next().await.is_none());
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn sdk_compatibility_decodes_incomplete_event_and_ends_stream() {
        let fixture = Fixture::start(FixtureReply::Sse(INCOMPLETE_STREAM)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        match next_event(&mut stream).await.unwrap() {
            WireEvent::Incomplete {
                sequence_number,
                response,
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(response.id, "resp_incomplete");
                assert_eq!(response.status, WireStatus::Incomplete);
                assert_eq!(
                    response.incomplete_details.unwrap().reason,
                    "max_output_tokens"
                );
            }
            event => panic!("unexpected incomplete event: {event:?}"),
        }
        assert!(stream.next().await.is_none());
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn sdk_compatibility_decodes_error_event_and_ends_stream() {
        let fixture = Fixture::start(FixtureReply::Sse(ERROR_STREAM)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        match next_event(&mut stream).await.unwrap() {
            WireEvent::Error {
                sequence_number,
                code,
                message,
            } => {
                assert_eq!(sequence_number, 1);
                assert_eq!(code.as_deref(), Some("stream_error"));
                assert_eq!(message, "stream failed");
            }
            event => panic!("unexpected error event: {event:?}"),
        }
        assert!(stream.next().await.is_none());
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn sdk_compatibility_makes_one_request_for_a_transient_response() {
        let fixture = Fixture::start(FixtureReply::Status {
            code: 429,
            body: r#"{"error":{"message":"retry later","type":"rate_limit_error","param":null,"code":"rate_limit"}}"#,
        })
        .await;

        assert!(open_stream(&fixture).await.is_err());
        sleep(Duration::from_millis(100)).await;
        assert_eq!(fixture.request_count(), 1);
    }

    #[tokio::test]
    async fn sdk_compatibility_reports_an_abrupt_stream_close() {
        let fixture = Fixture::start(FixtureReply::AbruptSse(ONE_DELTA)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        assert!(matches!(
            next_event(&mut stream).await.unwrap(),
            WireEvent::ReasoningSummaryDelta { delta, .. } if delta == "partial"
        ));
        assert!(next_event(&mut stream).await.is_err());
    }

    #[tokio::test]
    async fn sdk_compatibility_dropped_stream_closes_the_fixture_connection() {
        let fixture = Fixture::start(FixtureReply::IdleSse(ONE_DELTA)).await;
        let mut stream = open_stream(&fixture).await.unwrap();

        assert!(matches!(
            next_event(&mut stream).await.unwrap(),
            WireEvent::ReasoningSummaryDelta { delta, .. } if delta == "partial"
        ));
        drop(stream);

        fixture.wait_for_disconnect().await;
        assert_eq!(fixture.request_count(), 1);
    }
}
