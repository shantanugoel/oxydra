use std::collections::BTreeMap;
use std::io::{self, Write};

use provider::OpenAIProvider;
use types::{
    Context, Message, MessageRole, ModelCatalog, ModelId, Provider as _, ProviderId, StreamItem,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let prompt = std::env::args().nth(1).unwrap_or_else(|| {
        "Say hello and name one benefit of Rust async in one sentence.".to_owned()
    });

    let api_key = std::env::var("OPENAI_API_KEY").expect("set OPENAI_API_KEY to run this example");
    let catalog = ModelCatalog::from_pinned_snapshot().expect("pinned model catalog should parse");
    let provider = OpenAIProvider::new(
        ProviderId::from("openai"),
        ProviderId::from("openai"),
        api_key,
        String::new(),
        BTreeMap::new(),
        catalog,
    );
    let context = Context {
        provider: ProviderId::from("openai"),
        model: ModelId::from("gpt-4o-mini"),
        tools: vec![],
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
