use std::io::{self, Write};

use provider::{OpenAIConfig, OpenAIProvider};
use types::{Context, Message, MessageRole, ModelId, Provider as _, ProviderId, StreamItem};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args().nth(1).unwrap_or_else(|| {
        "Say hello and name one benefit of Rust async in one sentence.".to_owned()
    });

    let provider = OpenAIProvider::new(OpenAIConfig::default())?;
    let context = Context {
        provider: ProviderId::from("openai"),
        model: ModelId::from("gpt-4o-mini"),
        messages: vec![Message {
            role: MessageRole::User,
            content: Some(prompt),
            tool_calls: vec![],
            tool_call_id: None,
        }],
    };

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            let mut stream = provider.stream(&context, 64).await?;
            while let Some(item) = stream.recv().await {
                match item? {
                    StreamItem::Text(text) => {
                        print!("{text}");
                        io::stdout().flush()?;
                    }
                    StreamItem::ReasoningDelta(reasoning) => {
                        eprintln!("\n[reasoning_delta] {reasoning}");
                    }
                    StreamItem::ToolCallDelta(delta) => {
                        eprintln!("\n[tool_call_delta] {}", serde_json::to_string(&delta)?);
                    }
                    StreamItem::UsageUpdate(usage) => {
                        eprintln!("\n[usage_update] {}", serde_json::to_string(&usage)?);
                    }
                    StreamItem::ConnectionLost(message) => {
                        eprintln!("\n[connection_lost] {message}");
                    }
                    StreamItem::FinishReason(reason) => {
                        eprintln!("\n[finish_reason] {reason}");
                    }
                }
            }
            println!();
            Ok::<(), Box<dyn std::error::Error>>(())
        })?;

    Ok(())
}
