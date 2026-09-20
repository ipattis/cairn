//! A scripted executor, so a whole iteration — rollouts, curators, gate, commit — can
//! be tested without OpenCode, a provider or a network.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::{
    Executor, ExecutorInfo, RolloutSpec, SessionId, Transcript, TranscriptMessage,
    TranscriptToolCall,
};
use crate::Result;

/// Decides what a rollout produces, given the prompt and the skills that were injected.
pub type Responder = Arc<dyn Fn(&RolloutSpec) -> Transcript + Send + Sync>;

#[derive(Clone)]
pub struct MockExecutor {
    pub version: String,
    responder: Responder,
    /// Every rollout run, in order.
    pub calls: Arc<Mutex<Vec<RolloutSpec>>>,
    /// Sessions currently open, so a leak shows up in tests.
    open: Arc<Mutex<HashMap<String, String>>>,
    next_id: Arc<Mutex<u64>>,
}

impl MockExecutor {
    pub fn new(responder: Responder) -> Self {
        Self {
            version: "2.0.11".into(),
            responder,
            calls: Arc::new(Mutex::new(Vec::new())),
            open: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(Mutex::new(0)),
        }
    }

    /// Every rollout succeeds with a one-line answer.
    pub fn always(answer: impl Into<String> + Clone + Send + Sync + 'static) -> Self {
        Self::new(Arc::new(move |spec: &RolloutSpec| {
            simple_transcript(&spec.prompt, answer.clone().into(), true)
        }))
    }

    pub fn rollouts_run(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    pub fn open_sessions(&self) -> usize {
        self.open.lock().unwrap().len()
    }

    /// Models used, in order — proves per-run model swapping reached the executor.
    pub fn models_used(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.model.clone())
            .collect()
    }
}

/// A two-message transcript with one tool call, enough for condensing and labeling.
pub fn simple_transcript(prompt: &str, answer: String, tool_ok: bool) -> Transcript {
    Transcript {
        session: "mock".into(),
        messages: vec![
            TranscriptMessage {
                role: "user".into(),
                text: prompt.to_string(),
                ..Default::default()
            },
            TranscriptMessage {
                role: "assistant".into(),
                text: answer,
                reasoning: Some("mock reasoning".into()),
                tool_calls: vec![TranscriptToolCall {
                    name: "bash".into(),
                    input: serde_json::json!({"command": "run the check"}),
                    output: if tool_ok {
                        "ok".into()
                    } else {
                        "exit 1".into()
                    },
                    ok: tool_ok,
                }],
                tokens: Some(crate::model::Usage {
                    input_tokens: 1000,
                    output_tokens: 100,
                    cached_input_tokens: 900,
                }),
                ..Default::default()
            },
        ],
    }
}

#[async_trait]
impl Executor for MockExecutor {
    fn name(&self) -> &str {
        "mock"
    }

    async fn info(&self) -> Result<ExecutorInfo> {
        Ok(ExecutorInfo {
            name: "mock".into(),
            version: self.version.clone(),
        })
    }

    async fn create_session(&self, spec: &RolloutSpec) -> Result<SessionId> {
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        let id = format!("ses_{next}");
        self.open
            .lock()
            .unwrap()
            .insert(id.clone(), spec.title.clone());
        Ok(SessionId(id))
    }

    async fn prompt(&self, _session: &SessionId, spec: &RolloutSpec) -> Result<()> {
        self.calls.lock().unwrap().push(spec.clone());
        Ok(())
    }

    async fn transcript(&self, session: &SessionId) -> Result<Transcript> {
        let spec = self
            .calls
            .lock()
            .unwrap()
            .last()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("transcript requested before prompt"))?;
        let mut transcript = (self.responder)(&spec);
        transcript.session = session.0.clone();
        Ok(transcript)
    }

    async fn delete_session(&self, session: &SessionId) -> Result<()> {
        self.open.lock().unwrap().remove(&session.0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(prompt: &str, model: &str) -> RolloutSpec {
        RolloutSpec {
            agent: "wiki-inference".into(),
            model: model.into(),
            prompt: prompt.into(),
            work_dir: std::path::PathBuf::from("/tmp"),
            title: "t".into(),
        }
    }

    #[tokio::test]
    async fn run_rollout_cleans_up_its_session() {
        let executor = MockExecutor::always("done");
        let transcript = executor.run_rollout(&spec("fix it", "m1")).await.unwrap();
        assert_eq!(transcript.final_answer(), Some("done"));
        assert_eq!(executor.open_sessions(), 0);
        assert_eq!(executor.rollouts_run(), 1);
        assert_eq!(executor.models_used(), vec!["m1"]);
    }

    #[tokio::test]
    async fn the_responder_can_branch_on_the_prompt() {
        let executor = MockExecutor::new(Arc::new(|spec: &RolloutSpec| {
            let ok = spec.prompt.contains("easy");
            simple_transcript(&spec.prompt, if ok { "yes" } else { "no" }.into(), ok)
        }));
        let easy = executor.run_rollout(&spec("an easy task", "m")).await.unwrap();
        let hard = executor.run_rollout(&spec("a hard task", "m")).await.unwrap();
        assert_eq!(easy.final_answer(), Some("yes"));
        assert_eq!(hard.failed_tool_calls().len(), 1);
    }
}
