//! Tracer bullet: drive a full Infinity local agent system from a
//! forge-shaped provider adapter. If this runs, the adoption path is real.
use infinity_provider_protocol::{
    CompletionError, CompletionRequest, FinalResponse, ModelEntry, ModelProvider, ModelStream,
    StreamChunk, Usage,
};

/// Stands in for the adapter that will wrap forge's own `ModelProvider` —
/// the decision plane picks the model, this reports it to Infinity.
pub struct ForgeProviderAdapter;

#[async_trait::async_trait]
impl ModelProvider for ForgeProviderAdapter {
    async fn list_models(
        &self,
    ) -> Result<Vec<ModelEntry>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![ModelEntry {
            model_id: "forge/routed".into(),
            display_name: "forge (routed by the decision plane)".into(),
            context_window: 200_000,
            max_output_tokens: Some(8192),
            supports_image_input: true,
        }])
    }

    async fn invoke_model(
        &self,
        _model_id: &str,
        _request: CompletionRequest,
    ) -> Result<ModelStream, CompletionError> {
        let chunks = vec![
            Ok(StreamChunk::Text("routed by forge".into())),
            Ok(StreamChunk::Final(FinalResponse {
                usage: Some(Usage {
                    input_tokens: 7,
                    output_tokens: 3,
                    total_tokens: 10,
                    cached_input_tokens: 0,
                }),
            })),
        ];
        Ok(Box::pin(futures_util::stream::iter(chunks)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
    use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};

    #[tokio::test]
    async fn a_forge_provider_drives_an_infinity_local_thread() {
        // The local driver spawns `!Send` futures, so the whole system must
        // run inside a `LocalSet` pinned to one thread.
        tokio::task::LocalSet::new()
            .run_until(drive())
            .await;
    }

    async fn drive() {
        let provider: Arc<dyn ModelProvider> = Arc::new(ForgeProviderAdapter);
        let model = StaticModel::new(provider, "forge/routed")
            .await
            .expect("resolve the model through forge's catalogue");

        let system = AgentSystemBuilder::new_local(
            InMemoryConversationStore::new(),
            InMemoryStateStore::new(),
            model,
        )
        .start();

        let mut thread = system.thread_builder().launch().await;
        thread
            .send_user_text("does this seam hold?")
            .await
            .expect("deliver into the thread's mailbox");

        let mut saw_text = false;
        // Drain until the turn settles; the handle closes when it is done.
        while let Some(event) = thread.recv().await {
            let rendered = format!("{event:?}");
            println!("event: {rendered}");
            if rendered.contains("routed by forge") {
                saw_text = true;
            }
            if saw_text {
                break;
            }
        }
        assert!(saw_text, "the provider's text never reached the thread");
    }
}
