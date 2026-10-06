//! JSON-RPC over stdio. Serialized generation, cancellation, and bounded frames.
use super::*;
use std::sync::Arc;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    task::JoinHandle,
};
const FRAME_BYTES: usize = 128 * 1024;
type Generator = Arc<dyn Fn(String, String, RequestContext) -> Result<Value> + Send + Sync>;

fn tool(sizes: &[&str]) -> Value {
    let mut size_options = vec!["auto"];
    size_options.extend_from_slice(sizes);
    json!({"name":"generate_image","description":"Generate exactly one image with the explicitly configured optional provider. Never automatically repeat an ambiguous or failed generation.",
        "inputSchema":{"type":"object","properties":{"prompt":{"type":"string","minLength":1,"maxLength":16000},"size":{"type":"string","enum":size_options,"default":"auto"}},"required":["prompt"],"additionalProperties":false},
        "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":false,"openWorldHint":true}})
}
fn reply(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
fn failure(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}
fn tool_error(error: AdapterError) -> Value {
    json!({"isError":true,"content":[{"type":"text","text":format!("{}: {} No automatic repeat generation.",error.code,error.message)}]})
}
async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    bytes: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|_| error("MCP input could not be read."))?;
        if buffer.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(error("MCP input ended mid-frame."))
            };
        }
        let end = buffer
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(buffer.len());
        if bytes.len().saturating_add(end) > FRAME_BYTES {
            return Err(error("MCP input frame exceeds 128 KiB."));
        }
        let complete = buffer[end - 1] == b'\n';
        bytes.extend_from_slice(&buffer[..end]);
        reader.consume(end);
        if complete {
            return Ok(Some(std::mem::take(bytes)));
        }
    }
}
async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, value: &Value) -> Result<()> {
    let mut bytes =
        serde_json::to_vec(value).map_err(|_| error("MCP output could not be encoded."))?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .map_err(|_| error("MCP output was disconnected."))?;
    writer
        .flush()
        .await
        .map_err(|_| error("MCP output could not be flushed."))
}
struct Active {
    id: Value,
    context: RequestContext,
    task: JoinHandle<Result<Value>>,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.context.cancellation.cancel();
    }
}

pub(super) async fn serve(paths: ClientPaths) -> Result<()> {
    let status = registration::status(&paths)?;
    let enabled =
        status["enabled"] == true && status["pending"] != true && status["provider_valid"] == true;
    let definition = enabled.then(|| {
        let sizes: Vec<_> = status["supported_sizes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        tool(&sizes)
    });
    let generate: Generator = Arc::new(move |prompt, size, ctx| {
        let image = super::generate(&paths, &prompt, &size, ctx)?;
        Ok(
            json!({"content":[image.content(),{"type":"text","text":format!("Generated one {} image ({}x{}).",image.mime_type,image.width,image.height)}]}),
        )
    });
    serve_io(
        BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        definition,
        generate,
    )
    .await
}

async fn serve_io<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    mut input: R,
    mut output: W,
    definition: Option<Value>,
    generate: Generator,
) -> Result<()> {
    let enabled = definition.is_some();
    let mut initialized = false;
    let mut pending_frame = Vec::new();
    let mut active: Option<Active> = None;
    // Completed request IDs are never dispatched twice within this MCP session.
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let frame = if let Some(current) = &mut active {
            tokio::select! {
                result=&mut current.task=>{
                    let result=result.unwrap_or_else(|_|Err(error("The image worker failed; do not repeat generation automatically.")));
                    let id=current.id.clone();
                    write_frame(&mut output,&reply(id,result.unwrap_or_else(tool_error))).await?;
                    active=None;continue;
                }
                frame=read_frame(&mut input, &mut pending_frame)=>frame?,
            }
        } else {
            read_frame(&mut input, &mut pending_frame).await?
        };
        let Some(frame) = frame else {
            break;
        };
        let value: Value = match serde_json::from_slice(&frame) {
            Ok(value) => value,
            Err(_) => {
                write_frame(&mut output, &failure(Value::Null, -32700, "Invalid JSON.")).await?;
                continue;
            }
        };
        let method = value["method"].as_str().unwrap_or("");
        if value.get("id").is_none() {
            if method == "notifications/cancelled"
                && let Some(current) = &active
                && value["params"]["requestId"] == current.id
            {
                current.context.cancellation.cancel();
            }
            continue;
        }
        let id = value["id"].clone();
        if value["jsonrpc"] != "2.0"
            || !(id.as_str().is_some_and(|id| id.len() <= 128) || id.is_i64() || id.is_u64())
        {
            write_frame(
                &mut output,
                &failure(Value::Null, -32600, "Invalid JSON-RPC request."),
            )
            .await?;
            continue;
        }
        if active.as_ref().is_some_and(|current| current.id == id) {
            // A second reply with the active ID would falsely complete the original
            // billable call. Reject the collision without cancelling or relabelling it.
            write_frame(&mut output, &failure(Value::Null, -32600,
                "Request ID is already active; the original request is still running. No additional generation was submitted.")).await?;
            continue;
        }
        let result = match method {
            "initialize" => {
                initialized = true;
                let version = value["params"]["protocolVersion"]
                    .as_str()
                    .filter(|v| matches!(*v, "2025-03-26" | "2025-06-18" | "2025-11-25"))
                    .unwrap_or("2025-06-18");
                reply(
                    id,
                    json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"github-adapter-image","version":env!("CARGO_PKG_VERSION")},"instructions":"This optional provider is independent of the coding provider. Each generate_image call performs at most one generation. Never automatically repeat a failed or ambiguous dispatch."}),
                )
            }
            "ping" => reply(id, json!({})),
            _ if !initialized => failure(id, -32000, "Initialize the MCP session first."),
            "tools/list" => reply(id, json!({"tools":definition.iter().collect::<Vec<_>>()})),
            "tools/call" => {
                let args = &value["params"]["arguments"];
                if value["params"]["name"] != "generate_image" || !enabled {
                    failure(
                        id,
                        -32602,
                        "The optional image tool is unavailable; configure it explicitly.",
                    )
                } else if !args.is_object()
                    || args
                        .as_object()
                        .unwrap()
                        .keys()
                        .any(|key| !matches!(key.as_str(), "prompt" | "size"))
                    || !args["prompt"].is_string()
                    || args.get("size").is_some_and(|v| !v.is_string())
                {
                    failure(id, -32602, "Invalid image arguments.")
                } else if active.is_some() {
                    reply(
                        id,
                        tool_error(AdapterError::new(
                            429,
                            "image_busy",
                            "Another image request is active. Nothing was submitted.",
                        )),
                    )
                } else if ids.len() >= 1024 || !ids.insert(id.to_string()) {
                    failure(
                        id,
                        -32600,
                        "Duplicate image request ID or session request limit; no generation was submitted.",
                    )
                } else {
                    let context = RequestContext::new(Duration::from_secs(240))?;
                    let ctx = context.clone();
                    let prompt = args["prompt"].as_str().unwrap().to_owned();
                    let size = args["size"].as_str().unwrap_or("auto").to_owned();
                    let run = generate.clone();
                    active = Some(Active {
                        id,
                        context,
                        task: tokio::task::spawn_blocking(move || run(prompt, size, ctx)),
                    });
                    continue;
                }
            }
            _ => failure(id, -32601, "Method not found."),
        };
        write_frame(&mut output, &result).await?;
    }
    if let Some(mut current) = active {
        current.context.cancellation.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut current.task).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_tool() -> Option<Value> {
        Some(tool(&["1024x1024", "1536x1024", "1024x1536"]))
    }

    #[test]
    fn tool_schema_advertises_the_configured_models_sizes_and_auto_default() {
        let provider = ImageConfig {
            endpoint: "https://images.example.invalid/generation".into(),
            model: "qwen-image-max".into(),
            protocol: ImageProtocol::QwenMessages,
        };
        let definition = tool(provider.supported_sizes());
        let size = &definition["inputSchema"]["properties"]["size"];
        assert_eq!(size["default"], "auto");
        let choices = size["enum"].as_array().unwrap();
        assert!(choices.contains(&json!("1328x1328")));
        assert!(choices.contains(&json!("auto")));
        assert!(!choices.contains(&json!("1024x1024")));
    }
    #[tokio::test]
    async fn initialize_and_list_never_generate_and_disabled_tools_stay_hidden() {
        for enabled in [true, false] {
            let bytes=b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\"}}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
            let mut output = Vec::new();
            serve_io(
                BufReader::new(&bytes[..]),
                &mut output,
                if enabled { test_tool() } else { None },
                Arc::new(|_, _, _| panic!("readiness must never generate")),
            )
            .await
            .unwrap();
            let frames: Vec<Value> = output
                .split(|b| *b == b'\n')
                .filter(|v| !v.is_empty())
                .map(|v| serde_json::from_slice(v).unwrap())
                .collect();
            assert_eq!(frames[0]["result"]["protocolVersion"], "2025-06-18");
            assert_eq!(
                frames[1]["result"]["tools"].as_array().unwrap().len(),
                usize::from(enabled)
            );
        }
    }
    #[tokio::test]
    async fn cancellation_is_processed_during_generation_and_never_replayed() {
        let (mut client, server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(server);
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let service = tokio::spawn(serve_io(
            BufReader::new(read),
            write,
            test_tool(),
            Arc::new(move |_, _, ctx| {
                calls.fetch_add(1, Ordering::SeqCst);
                while !ctx.cancellation.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error("Synthetic cancellation"))
            }),
        ));
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"generate_image\",\"arguments\":{\"prompt\":\"synthetic\"}}}\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while count.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":2}}\n").await.unwrap();
        let mut reader = BufReader::new(client);
        read_frame(&mut reader, &mut Vec::new()).await.unwrap();
        let result: Value = serde_json::from_slice(
            &tokio::time::timeout(
                Duration::from_secs(3),
                read_frame(&mut reader, &mut Vec::new()),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(result["result"]["isError"], true);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(reader);
        service.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_completed_generation_does_not_discard_a_partial_rpc_frame_or_allow_replay() {
        let (mut client, server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(server);
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = release.clone();
        let service = tokio::spawn(serve_io(
            BufReader::new(read),
            write,
            test_tool(),
            Arc::new(move |_, _, ctx| {
                calls.fetch_add(1, Ordering::SeqCst);
                while !gate.load(Ordering::SeqCst) {
                    ctx.check()?;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(json!({"content":[{"type":"text","text":"synthetic result"}]}))
            }),
        ));
        client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"generate_image\",\"arguments\":{\"prompt\":\"synthetic\"}}}\n{\"jsonrpc\":\"2.0\",\"id\":3,").await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while count.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let mut reader = BufReader::new(client);
        read_frame(&mut reader, &mut Vec::new()).await.unwrap();
        release.store(true, Ordering::SeqCst);
        let result: Value = serde_json::from_slice(
            &tokio::time::timeout(
                Duration::from_secs(3),
                read_frame(&mut reader, &mut Vec::new()),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(result["id"], 2);
        reader.get_mut().write_all(b"\"method\":\"ping\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"generate_image\",\"arguments\":{\"prompt\":\"synthetic\"}}}\n").await.unwrap();
        let pong: Value = serde_json::from_slice(
            &read_frame(&mut reader, &mut Vec::new())
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(pong["id"], 3);
        assert!(pong.get("result").is_some());
        let duplicate: Value = serde_json::from_slice(
            &read_frame(&mut reader, &mut Vec::new())
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert!(duplicate.get("error").is_some());
        assert_eq!(count.load(Ordering::SeqCst), 1);
        drop(reader);
        service.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn active_generation_ids_cannot_be_shadowed_by_another_rpc_reply() {
        let (client, server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(server);
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let gate = release.clone();
        let service = tokio::spawn(serve_io(
            BufReader::new(read),
            write,
            test_tool(),
            Arc::new(move |_, _, ctx| {
                calls.fetch_add(1, Ordering::SeqCst);
                while !gate.load(Ordering::SeqCst) {
                    ctx.check()?;
                    std::thread::sleep(Duration::from_millis(2));
                }
                Ok(json!({"content":[{"type":"text","text":"original image result"}]}))
            }),
        ));
        let mut client = BufReader::new(client);
        client.get_mut().write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"generate_image\",\"arguments\":{\"prompt\":\"synthetic\"}}}\n").await.unwrap();
        read_frame(&mut client, &mut Vec::new()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while count.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let mut collisions = Vec::new();
        for method in ["tools/call", "tools/list", "ping", "initialize"] {
            let request = json!({"jsonrpc":"2.0","id":2,"method":method,"params":{"name":"generate_image","arguments":{"prompt":"must not run"}}});
            write_frame(client.get_mut(), &request).await.unwrap();
            let frame = tokio::time::timeout(
                Duration::from_secs(3),
                read_frame(&mut client, &mut Vec::new()),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
            collisions.push(serde_json::from_slice::<Value>(&frame).unwrap());
        }
        release.store(true, Ordering::SeqCst);
        let frame = tokio::time::timeout(
            Duration::from_secs(3),
            read_frame(&mut client, &mut Vec::new()),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        let original: Value = serde_json::from_slice(&frame).unwrap();
        drop(client);
        service.await.unwrap().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(original["id"], 2);
        assert_eq!(
            original["result"]["content"][0]["text"],
            "original image result"
        );
        for collision in collisions {
            assert_eq!(
                collision["id"],
                Value::Null,
                "Never report a different outcome for the still-running image request."
            );
            assert_eq!(collision["error"]["code"], -32600);
        }
    }
}
