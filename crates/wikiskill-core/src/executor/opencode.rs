//! OpenCode V2 (`opencode2`, 2.0.x) as the executor, over its HTTP server.
//!
//! The rationale's plan is to generate a Rust client from the pinned OpenAPI spec with
//! `progenitor`. Until that spec is vendored into the repo, this hand-written client
//! stands in: every path and payload shape lives in [`paths`] and [`wire`] so the
//! generated client can replace them in one place.
//!
//! Three things differ from the V1 client this replaces, and each one is load-bearing:
//!
//! * **Basic auth is mandatory.** V2 refuses an unauthenticated request even on localhost,
//!   as user `opencode` with a password the server generates — or, as here, one the daemon
//!   generates and passes in through `OPENCODE_SERVER_PASSWORD`. This is strictly better
//!   than V1, whose localhost server was open to every process on the machine.
//! * **Prompting is asynchronous.** `POST .../prompt` returns as soon as the turn is
//!   queued, so completion is a separate step: see [`OpencodeClient::wait_for_idle`].
//! * **Permissions are per session.** The ruleset travels with the session over the API
//!   rather than living only in a config file the rollout can write to. That matters
//!   because an unmatched action defaults to `ask`, and nothing answers a question in a
//!   headless rollout — see [`rollout_permissions`].
//!
//! The pinned version is enforced at startup because this whole HTTP surface is labelled
//! experimental in V2's own spec, and the completion route is under `/api/experimental/`.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{
    Executor, ExecutorInfo, RolloutSpec, SessionId, Transcript, TranscriptMessage,
    TranscriptToolCall,
};
use crate::model::Usage;
use crate::vault::Skill;
use crate::Result;

/// The only username V2's server accepts. The password is per-daemon-run and generated.
pub const AUTH_USER: &str = "opencode";

/// Endpoint paths of the pinned V2 server API.
pub mod paths {
    /// `{"version":"2.0.11","pid":…,"urls":[…],"paths":{…}}`. Unlike V1, which answered any
    /// unknown path with the web UI's HTML at status 200, V2 404s properly — so a wrong path
    /// here arrives as a status error naming itself rather than a JSON parse failure.
    pub const INFO: &str = "/api/info";
    /// How long a readiness probe may take. Deliberately short and separate from the client's
    /// hour-long rollout timeout: a server still starting up can accept the connection and then
    /// never answer, and a readiness loop that inherits the rollout timeout does not poll — it
    /// blocks on its first attempt for an hour, and the run it was starting stays `queued`.
    pub const HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    pub const SESSION: &str = "/api/session";
    pub fn session(id: &str) -> String {
        format!("/api/session/{id}")
    }
    pub fn session_message(id: &str) -> String {
        format!("/api/session/{id}/message")
    }
    pub fn session_prompt(id: &str) -> String {
        format!("/api/session/{id}/prompt")
    }
    /// Blocks until the session is idle, then answers 204. Under `/api/experimental/`, which
    /// is why [`super::OpencodeClient::wait_for_idle`] can fall back to polling.
    pub fn session_wait(id: &str) -> String {
        format!("/api/experimental/session/{id}/wait")
    }
}

pub struct OpencodeClient {
    http: reqwest::Client,
    base_url: String,
    pinned_version: String,
    /// The server's own provider id for the deploy model. A property of this executor, not of a
    /// rollout: every rollout in a run reaches the same model through the same provider.
    model_provider: String,
    /// Basic-auth password for the supervised server. Never logged, never in argv.
    password: String,
}

impl OpencodeClient {
    pub fn new(
        base_url: impl Into<String>,
        pinned_version: impl Into<String>,
        model_provider: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                // A rollout can legitimately run for many minutes, and the wait route blocks
                // for the whole turn; the loop applies its own per-rollout timeout on top.
                .timeout(Duration::from_secs(3600))
                .build()?,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            pinned_version: pinned_version.into(),
            model_provider: model_provider.into(),
            password: password.into(),
        })
    }

    /// Every request carries the Basic credentials; V2 401s without them.
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}", self.base_url, path))
            .basic_auth(AUTH_USER, Some(&self.password))
    }

    /// Fails when the server has left the pinned line, e.g. a V1 binary on the port.
    pub async fn check_pinned(&self) -> Result<ExecutorInfo> {
        let info = self.info().await?;
        if !version_matches(&info.version, &self.pinned_version) {
            anyhow::bail!(
                "opencode at {} reports version {}, which is outside the pinned line {}. \
                 This client speaks the V2 API, which V2 itself marks experimental: pin the \
                 release or update the client.",
                self.base_url,
                info.version,
                self.pinned_version
            );
        }
        Ok(info)
    }

    async fn send_prompt(&self, session: &SessionId, spec: &RolloutSpec) -> Result<()> {
        let response = self
            .request(reqwest::Method::POST, &paths::session_prompt(&session.0))
            .json(&wire::PromptRequest { text: &spec.prompt })
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!(
                "opencode prompt failed ({status}): {}",
                body.chars().take(1000).collect::<String>()
            );
        }
        Ok(())
    }

    /// Blocks until the session's turn is over, and reports the outcome it ended on.
    ///
    /// Two mechanisms, because the first one is experimental: the blocking wait route, then a
    /// poll of the message list for the `idle` entry the server appends when a turn finishes.
    /// The idle *message* is the signal rather than the session's `time.idle` field because a
    /// freshly created session is already idle — a rollout is one prompt in one session, so any
    /// idle entry in the list means this turn is done.
    ///
    /// Unbounded on purpose: the caller wraps each rollout in `loop.rollout_timeout_secs`, and a
    /// second deadline here would only disagree with it.
    pub async fn wait_for_idle(&self, session: &SessionId) -> Result<Option<String>> {
        let response = self
            .request(reqwest::Method::POST, &paths::session_wait(&session.0))
            .send()
            .await;
        match response {
            Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                anyhow::bail!(
                    "opencode rejected the server password; the daemon and the server it \
                     supervised disagree about OPENCODE_SERVER_PASSWORD"
                )
            }
            Ok(response) if response.status().is_success() => {}
            Ok(response) => tracing::warn!(
                status = %response.status(),
                "the experimental wait route did not answer; polling for the idle message instead"
            ),
            Err(e) => tracing::warn!(
                "the experimental wait route failed ({e}); polling for the idle message instead"
            ),
        }
        self.poll_until_idle(session).await
    }

    async fn poll_until_idle(&self, session: &SessionId) -> Result<Option<String>> {
        loop {
            if let Some(outcome) = idle_outcome(&self.messages(session).await?) {
                return Ok(Some(outcome));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn messages(&self, session: &SessionId) -> Result<String> {
        Ok(self
            .request(reqwest::Method::GET, &paths::session_message(&session.0))
            // Oldest first, so a transcript reads in the order it happened.
            .query(&[("order", "asc")])
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    }
}

/// True when `version` is in the `pinned` line, e.g. `2.0.11` in `2.0`.
pub fn version_matches(version: &str, pinned: &str) -> bool {
    let v: Vec<&str> = version.trim_start_matches('v').split('.').collect();
    let p: Vec<&str> = pinned.trim_start_matches('v').split('.').collect();
    if p.is_empty() || v.len() < p.len() {
        return false;
    }
    v.iter().zip(p.iter()).all(|(a, b)| a == b)
}

/// The rollout permission ruleset: ordered, and the **last matching rule wins**.
///
/// It opens with `*`/`*`/`allow` rather than enumerating actions, and that is the important
/// line. Anything no rule matches resolves to `ask`, which in a headless rollout means the turn
/// sits on a permission request nobody will ever answer until the loop's rollout timeout expires
/// and scores it 0 — and V2's *default* policy asks about two things a rollout does routinely:
/// reading a `.env` in its own workspace, and touching a directory outside the session location.
/// Action names are plain strings in V2 and plugins may add more, so an allow-all opener is also
/// the only form that cannot rot.
///
/// What follows are the deliberate exceptions, each one a `deny` rather than an `ask` for the
/// same reason — a refusal is something the model can act on:
///
/// * `skill`, because the loop injects every active `SKILL.md` into the agent file itself,
///   exactly as the paper does. The native skill tool would both duplicate the instructions and
///   let the injected block differ between rollouts, which costs the prompt cache and breaks the
///   measurement it exists to make.
/// * `external_directory`, `webfetch` and `websearch`, because a rollout is measured on what it
///   does in its own workspace copy. These are advice, not the boundary: the Seatbelt profile
///   is, and a bash tool can walk around anything OpenCode believes about paths.
/// * `question`, because there is no interactive client to answer one.
pub fn rollout_permissions() -> serde_json::Value {
    let rule = |action: &str, resource: &str, effect: &str| {
        serde_json::json!({ "action": action, "resource": resource, "effect": effect })
    };
    serde_json::Value::Array(vec![
        rule("*", "*", "allow"),
        // Overriding V2's base policy, which asks about these two. The workspace is a copy made
        // for this rollout; the daemon's redaction pass, not a prompt, is what keeps a secret
        // out of the vault.
        rule("read", "*.env", "allow"),
        rule("read", "*.env.*", "allow"),
        rule("external_directory", "*", "deny"),
        rule("skill", "*", "deny"),
        rule("question", "*", "deny"),
        rule("webfetch", "*", "deny"),
        rule("websearch", "*", "deny"),
    ])
}

/// Wire shapes of the pinned V2 API.
mod wire {
    use super::*;

    /// `version` is required: a body without it is not this endpoint's.
    #[derive(Debug, Deserialize)]
    pub struct ServerInfo {
        pub version: String,
    }

    /// Everything about a rollout that V2 sets at session creation rather than per prompt:
    /// the agent, the model, the working directory, and the permission ruleset.
    #[derive(Debug, Serialize)]
    pub struct CreateSession<'a> {
        pub title: &'a str,
        pub agent: &'a str,
        pub model: ModelRef<'a>,
        pub location: Location<'a>,
        pub permissions: serde_json::Value,
    }

    /// `{id, providerID}`. The body schema sets `additionalProperties: false`, so a bare model
    /// string is a 400 rather than a fallback.
    #[derive(Debug, Serialize)]
    pub struct ModelRef<'a> {
        pub id: &'a str,
        #[serde(rename = "providerID")]
        pub provider_id: &'a str,
    }

    /// The working directory. In V1 this was a query parameter that was silently ignored in the
    /// body; in V2 it is a body field, and getting it wrong would run every rollout in the
    /// server's own cwd and score all of them against the same tree.
    #[derive(Debug, Serialize)]
    pub struct Location<'a> {
        pub directory: &'a str,
    }

    /// Single-field envelope V2 wraps its resources in.
    #[derive(Debug, Deserialize)]
    pub struct Data<T> {
        pub data: T,
    }

    #[derive(Debug, Deserialize)]
    pub struct SessionInfo {
        pub id: String,
    }

    #[derive(Debug, Serialize)]
    pub struct PromptRequest<'a> {
        pub text: &'a str,
    }

    #[derive(Debug, Deserialize)]
    pub struct Messages {
        #[serde(default)]
        pub data: Vec<Message>,
    }

    /// The message list is a tagged union; the variants this client does not read are still
    /// parsed so one unknown kind cannot fail a whole transcript.
    #[derive(Debug, Deserialize)]
    #[serde(tag = "type")]
    pub enum Message {
        #[serde(rename = "user")]
        User(User),
        #[serde(rename = "assistant")]
        Assistant(Assistant),
        #[serde(rename = "idle")]
        Idle(Idle),
        #[serde(other)]
        Other,
    }

    #[derive(Debug, Deserialize)]
    pub struct User {
        #[serde(default)]
        pub text: String,
    }

    /// `succeeded`, `failed` or `interrupted`. Appended when a turn goes idle.
    #[derive(Debug, Deserialize)]
    pub struct Idle {
        pub outcome: String,
    }

    #[derive(Debug, Deserialize)]
    pub struct Assistant {
        #[serde(default)]
        pub model: Option<ModelId>,
        #[serde(default)]
        pub content: Vec<Content>,
        #[serde(default)]
        pub tokens: Option<Tokens>,
        /// Set when the turn itself failed — a context overflow, a provider auth error.
        #[serde(default)]
        pub error: Option<StructuredError>,
    }

    #[derive(Debug, Deserialize)]
    pub struct ModelId {
        pub id: String,
    }

    #[derive(Debug, Deserialize)]
    pub struct StructuredError {
        #[serde(default)]
        pub message: String,
    }

    #[derive(Debug, Deserialize)]
    pub struct Tokens {
        #[serde(default)]
        pub input: u64,
        #[serde(default)]
        pub output: u64,
        #[serde(default)]
        pub cache: Option<Cache>,
    }

    #[derive(Debug, Deserialize)]
    pub struct Cache {
        #[serde(default)]
        pub read: u64,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type")]
    pub enum Content {
        #[serde(rename = "text")]
        Text { #[serde(default)] text: String },
        #[serde(rename = "reasoning")]
        Reasoning { #[serde(default)] text: String },
        #[serde(rename = "tool")]
        Tool(Tool),
        #[serde(other)]
        Other,
    }

    #[derive(Debug, Deserialize)]
    pub struct Tool {
        pub name: String,
        #[serde(default)]
        pub state: Option<ToolState>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "status")]
    pub enum ToolState {
        #[serde(rename = "completed")]
        Completed {
            #[serde(default)]
            input: serde_json::Value,
            #[serde(default)]
            content: Vec<ToolContent>,
        },
        #[serde(rename = "error")]
        Error {
            #[serde(default)]
            input: serde_json::Value,
            error: StructuredError,
        },
        /// `streaming` and `running`. A transcript is fetched after the turn is idle, so these
        /// only appear for a call that never finished.
        #[serde(other)]
        Unfinished,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "type")]
    pub enum ToolContent {
        #[serde(rename = "text")]
        Text { #[serde(default)] text: String },
        #[serde(rename = "file")]
        File { uri: String },
        #[serde(other)]
        Other,
    }

    impl ToolState {
        pub fn into_call(self, name: String) -> TranscriptToolCall {
            match self {
                ToolState::Completed { input, content } => TranscriptToolCall {
                    name,
                    input,
                    output: render_content(&content),
                    ok: true,
                },
                ToolState::Error { input, error } => TranscriptToolCall {
                    name,
                    input,
                    output: error.message,
                    ok: false,
                },
                ToolState::Unfinished => TranscriptToolCall {
                    name,
                    input: serde_json::Value::Null,
                    output: "the tool call never finished".into(),
                    ok: false,
                },
            }
        }
    }

    /// Tool output is a list of parts in V2, not one string. A file part carries a URI rather
    /// than the bytes, so it is named instead of inlined.
    fn render_content(content: &[ToolContent]) -> String {
        let mut out = String::new();
        for part in content {
            let rendered = match part {
                ToolContent::Text { text } => text.clone(),
                ToolContent::File { uri } => format!("[file {uri}]"),
                ToolContent::Other => continue,
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&rendered);
        }
        out
    }
}

/// The outcome of the `idle` entry in a message list, if the turn has finished.
///
/// Public so the polling contract can be tested without a server. Returns `None` while the
/// turn is still running, which is the whole point: an absent idle entry is not an error.
pub fn idle_outcome(body: &str) -> Option<String> {
    let messages: wire::Messages = serde_json::from_str(body).ok()?;
    messages.data.into_iter().rev().find_map(|message| match message {
        wire::Message::Idle(idle) => Some(idle.outcome),
        _ => None,
    })
}

/// Maps the server's message list into our transcript format. Public so a captured response
/// body can be replayed against it when the spec moves.
pub fn parse_messages(session: &str, body: &str) -> Result<Transcript> {
    let parsed: wire::Messages = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("opencode: unparseable message list: {e}"))?;

    let mut messages = Vec::new();
    for entry in parsed.data {
        match entry {
            wire::Message::User(user) => messages.push(TranscriptMessage {
                role: "user".into(),
                text: user.text,
                ..Default::default()
            }),
            wire::Message::Assistant(assistant) => {
                let mut message = TranscriptMessage {
                    role: "assistant".into(),
                    model: assistant.model.map(|m| m.id),
                    tokens: assistant.tokens.map(|t| Usage {
                        input_tokens: t.input,
                        output_tokens: t.output,
                        cached_input_tokens: t.cache.map(|c| c.read).unwrap_or(0),
                    }),
                    ..Default::default()
                };
                let mut text = String::new();
                let mut reasoning = String::new();
                for part in assistant.content {
                    match part {
                        wire::Content::Text { text: t } => push_line(&mut text, &t),
                        wire::Content::Reasoning { text: t } => push_line(&mut reasoning, &t),
                        wire::Content::Tool(tool) => {
                            let name = tool.name;
                            message.tool_calls.push(match tool.state {
                                Some(state) => state.into_call(name),
                                None => TranscriptToolCall {
                                    name,
                                    ok: false,
                                    ..Default::default()
                                },
                            });
                        }
                        wire::Content::Other => {}
                    }
                }
                // A turn that failed outright has no text at all, and a transcript whose final
                // answer is silence tells the curators nothing about why.
                if let Some(error) = assistant.error {
                    push_line(&mut text, &format!("[model error] {}", error.message));
                }
                message.text = text;
                if !reasoning.is_empty() {
                    message.reasoning = Some(reasoning);
                }
                messages.push(message);
            }
            // Idle is the completion signal, not part of the conversation; the rest are
            // bookkeeping (agent/model switches, compaction) the curators do not read.
            wire::Message::Idle(_) | wire::Message::Other => {}
        }
    }

    Ok(Transcript {
        session: session.to_string(),
        messages,
    })
}

fn push_line(buffer: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !buffer.is_empty() {
        buffer.push('\n');
    }
    buffer.push_str(text);
}

#[async_trait]
impl Executor for OpencodeClient {
    fn name(&self) -> &str {
        "opencode"
    }

    async fn info(&self) -> Result<ExecutorInfo> {
        let body = self
            .request(reqwest::Method::GET, paths::INFO)
            .timeout(paths::HEALTH_TIMEOUT)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("opencode at {} is unreachable: {e}", self.base_url))?
            .error_for_status()
            .map_err(|e| {
                anyhow::anyhow!(
                    "opencode at {}{} answered {e}; a 401 here means the server was started \
                     with a different OPENCODE_SERVER_PASSWORD than this client holds",
                    self.base_url,
                    paths::INFO
                )
            })?
            .text()
            .await?;
        let info: wire::ServerInfo = serde_json::from_str(&body).map_err(|e| {
            anyhow::anyhow!("opencode: unparseable {} response ({e})", paths::INFO)
        })?;
        Ok(ExecutorInfo {
            name: "opencode".into(),
            version: info.version,
        })
    }

    async fn create_session(&self, spec: &RolloutSpec) -> Result<SessionId> {
        let response = self
            .request(reqwest::Method::POST, paths::SESSION)
            .json(&wire::CreateSession {
                title: &spec.title,
                agent: &spec.agent,
                model: wire::ModelRef {
                    id: &spec.model,
                    provider_id: &self.model_provider,
                },
                location: wire::Location {
                    directory: &spec.work_dir.to_string_lossy(),
                },
                permissions: rollout_permissions(),
            })
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "opencode session creation failed ({status}): {}",
                body.chars().take(1000).collect::<String>()
            );
        }
        let session: wire::Data<wire::SessionInfo> = serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("opencode: unparseable session response: {e}"))?;
        Ok(SessionId(session.data.id))
    }

    async fn prompt(&self, session: &SessionId, spec: &RolloutSpec) -> Result<()> {
        self.send_prompt(session, spec).await?;
        match self.wait_for_idle(session).await?.as_deref() {
            Some("succeeded") | None => {}
            // Not an error: an error here would abort the whole run, and one task the model
            // could not finish is a rollout that scores badly, not a broken harness. The
            // verifier reads the work tree either way, and the reason is in the transcript.
            Some(outcome) => tracing::warn!(
                %session,
                task = %spec.title,
                "the rollout session ended `{outcome}`; scoring the work tree as it stands"
            ),
        }
        Ok(())
    }

    async fn transcript(&self, session: &SessionId) -> Result<Transcript> {
        parse_messages(&session.0, &self.messages(session).await?)
    }

    async fn delete_session(&self, session: &SessionId) -> Result<()> {
        self.request(reqwest::Method::DELETE, &paths::session(&session.0))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Renders `agents/wiki-inference.md`: the rollout agent, with every active SKILL.md
/// concatenated into its body.
///
/// Deliberately almost bare frontmatter. The model and the permission ruleset are set per
/// session over the API, where the rollout cannot reach them — this file lives inside the
/// rollout's own writable `HOME`, so anything security-relevant stated here would be advice
/// rather than a control. The block is byte-identical across an iteration's rollouts so
/// prompt caching can absorb it.
pub fn render_agent_file(agent: &str, skills: &[Skill]) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("description: WikiSkill rollouts with injected skills\n");
    out.push_str("---\n");
    out.push_str(&format!(
        "<!-- generated by wikiskilld for agent `{agent}`; do not edit by hand -->\n"
    ));

    if skills.is_empty() {
        out.push_str(
            "\nYou have no additional skills. Solve the task with the tools you have.\n",
        );
        return out;
    }

    out.push_str("\n# Skills\n\nApply these where they fit the task at hand.\n");
    for skill in skills {
        out.push_str(&format!("\n## {}\n\n", skill.name));
        out.push_str(skill.body.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_version_line_accepts_patches_only() {
        assert!(version_matches("2.0.11", "2.0"));
        assert!(version_matches("v2.0.0", "2.0"));
        assert!(!version_matches("2.1.0", "2.0"));
        // The version this client replaced. A V1 binary on the port must be refused, not
        // talked to: it answers unknown paths with HTML at status 200.
        assert!(!version_matches("1.18.31", "2.0"));
        assert!(!version_matches("2.0", "2.0.11"));
        assert!(!version_matches("unknown", "2.0"));
    }

    /// The shape of a real V2 message list: envelope, tagged messages, content parts, and
    /// tool output as a list of parts rather than a string.
    fn message_list() -> &'static str {
        r#"{"data": [
          {"type": "user", "id": "msg_1", "time": {"created": 1}, "text": "fix it"},
          {"type": "agent-switched", "id": "msg_2", "time": {"created": 2}},
          {"type": "assistant", "id": "msg_3", "time": {"created": 3},
           "agent": "wiki-inference",
           "model": {"id": "deepseek-v4-pro", "providerID": "wikiskill-deploy"},
           "tokens": {"input": 900, "output": 50, "reasoning": 10,
                      "cache": {"read": 850, "write": 0}},
           "content": [
             {"type": "reasoning", "text": "the ordering assert is wrong"},
             {"type": "tool", "id": "t1", "name": "shell", "time": {"created": 4},
              "state": {"status": "error", "input": {"command": "cargo test"},
                        "error": {"type": "APIError", "message": "exit 101"}}},
             {"type": "tool", "id": "t2", "name": "edit", "time": {"created": 5},
              "state": {"status": "completed", "input": {"path": "src/lib.rs"},
                        "content": [{"type": "text", "text": "ok"}]}},
             {"type": "text", "text": "done"}
           ]},
          {"type": "idle", "id": "msg_4", "time": {"created": 6}, "outcome": "succeeded"}
        ], "cursor": {}}"#
    }

    #[test]
    fn parses_reasoning_tool_calls_and_tokens() {
        let transcript = parse_messages("ses_1", message_list()).unwrap();
        // The user message and the assistant message; the switch and the idle entry are not
        // conversation.
        assert_eq!(transcript.messages.len(), 2);
        let assistant = &transcript.messages[1];
        assert_eq!(assistant.reasoning.as_deref(), Some("the ordering assert is wrong"));
        assert_eq!(assistant.text, "done");
        assert_eq!(assistant.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(assistant.tool_calls.len(), 2);
        assert!(!assistant.tool_calls[0].ok);
        assert_eq!(assistant.tool_calls[0].output, "exit 101");
        assert!(assistant.tool_calls[1].ok);
        assert_eq!(assistant.tool_calls[1].output, "ok");
        assert_eq!(transcript.usage().cached_input_tokens, 850);
        assert_eq!(transcript.final_answer(), Some("done"));
    }

    #[test]
    fn the_idle_entry_is_the_completion_signal() {
        assert_eq!(idle_outcome(message_list()).as_deref(), Some("succeeded"));
        // Still running: no idle entry yet. This must not look like an error, or every
        // rollout would fail on its first poll.
        let running = r#"{"data": [{"type": "user", "id": "msg_1",
                          "time": {"created": 1}, "text": "fix it"}], "cursor": {}}"#;
        assert_eq!(idle_outcome(running), None);
        assert_eq!(idle_outcome("not json"), None);
    }

    /// A turn that failed produces no text, and a transcript whose final answer is silence
    /// tells the curators nothing. The provider's reason belongs in the trace.
    #[test]
    fn a_failed_turn_carries_its_reason_into_the_transcript() {
        let body = r#"{"data": [
          {"type": "assistant", "id": "msg_1", "time": {"created": 1}, "agent": "a",
           "model": {"id": "m", "providerID": "p"}, "content": [],
           "error": {"type": "ContextOverflowError", "message": "context exceeded"}},
          {"type": "idle", "id": "msg_2", "time": {"created": 2}, "outcome": "failed"}
        ], "cursor": {}}"#;
        let transcript = parse_messages("s", body).unwrap();
        assert_eq!(
            transcript.final_answer(),
            Some("[model error] context exceeded")
        );
        assert_eq!(idle_outcome(body).as_deref(), Some("failed"));
    }

    #[test]
    fn unknown_message_and_content_types_are_ignored_not_fatal() {
        let body = r#"{"data": [
          {"type": "something-v2-added-later", "id": "msg_1", "time": {"created": 1}},
          {"type": "assistant", "id": "msg_2", "time": {"created": 2}, "agent": "a",
           "model": {"id": "m", "providerID": "p"},
           "content": [{"type": "patch", "hunks": []}, {"type": "text", "text": "hi"}]}
        ], "cursor": {}}"#;
        let transcript = parse_messages("s", body).unwrap();
        assert_eq!(transcript.messages.len(), 1);
        assert_eq!(transcript.messages[0].text, "hi");
    }

    #[test]
    fn a_bad_body_is_an_error_with_context() {
        let err = parse_messages("s", "not json").unwrap_err().to_string();
        assert!(err.contains("unparseable message list"), "{err}");
    }

    /// V2's whole-value wildcards: `*` is zero or more characters *including* `/`, `?` is
    /// exactly one, everything else is literal.
    fn wildcard_matches(pattern: &str, value: &str) -> bool {
        match pattern.chars().next() {
            None => value.is_empty(),
            Some('*') => {
                let rest = &pattern[1..];
                (0..=value.len())
                    .filter(|i| value.is_char_boundary(*i))
                    .any(|i| wildcard_matches(rest, &value[i..]))
            }
            Some('?') => {
                let mut chars = value.chars();
                chars.next().is_some() && wildcard_matches(&pattern[1..], chars.as_str())
            }
            Some(c) => value.strip_prefix(c).is_some_and(|v| wildcard_matches(&pattern[1..], v)),
        }
    }

    /// V2's base policy, which every agent starts with and our rules are appended after.
    /// Reproduced from the permissions guide so the test resolves what the server will.
    fn base_policy() -> Vec<serde_json::Value> {
        serde_json::json!([
            { "action": "*", "resource": "*", "effect": "allow" },
            { "action": "external_directory", "resource": "*", "effect": "ask" },
            { "action": "read", "resource": "*.env", "effect": "ask" },
            { "action": "read", "resource": "*.env.*", "effect": "ask" },
            { "action": "read", "resource": "*.env.example", "effect": "allow" },
        ])
        .as_array()
        .unwrap()
        .clone()
    }

    /// Resolves one request the way V2 does: last matching rule wins, `ask` if none match.
    fn resolve(action: &str, resource: &str) -> String {
        let mut rules = base_policy();
        rules.extend(rollout_permissions().as_array().unwrap().iter().cloned());
        rules
            .iter()
            .rev()
            .find(|r| {
                wildcard_matches(r["action"].as_str().unwrap(), action)
                    && wildcard_matches(r["resource"].as_str().unwrap(), resource)
            })
            .map(|r| r["effect"].as_str().unwrap().to_string())
            .unwrap_or_else(|| "ask".into())
    }

    /// Nothing answers a question in a headless rollout: an `ask` is a turn that hangs until the
    /// loop's timeout expires and scores it 0. V2's *own* base policy asks about two things a
    /// rollout does routinely, so this resolves against the composed ruleset, not just ours.
    #[test]
    fn the_permission_ruleset_leaves_no_action_to_default_to_ask() {
        let expected = [
            ("read", "src/main.rs", "allow"),
            // The two the base policy asks about.
            ("read", ".env", "allow"),
            ("read", "crates/api/.env.local", "allow"),
            ("edit", "src/main.rs", "allow"),
            ("shell", "cargo test --all", "allow"),
            ("glob", "**/*.rs", "allow"),
            ("grep", "fn main", "allow"),
            ("subagent", "general", "allow"),
            // Code Mode and MCP tools name themselves; the allow-all opener is what covers an
            // action this code has never heard of.
            ("execute", "*", "allow"),
            ("playwright_navigate", "*", "allow"),
            // The measurement invariant: skills arrive by injection into the agent file only.
            ("skill", "deny-rules-last", "deny"),
            ("question", "*", "deny"),
            ("webfetch", "https://example.com", "deny"),
            ("websearch", "how do i fix this test", "deny"),
            ("external_directory", "/Users/me/Vault/*", "deny"),
        ];
        for (action, resource, effect) in expected {
            assert_eq!(
                resolve(action, resource),
                effect,
                "{action} on {resource} must resolve to {effect}"
            );
        }
    }

    #[test]
    fn the_wildcard_matcher_behaves_the_way_the_guide_describes() {
        assert!(wildcard_matches("*", ""));
        assert!(wildcard_matches("*.env", ".env"));
        assert!(wildcard_matches("*.env", "a/b/.env"), "`*` spans `/`");
        assert!(!wildcard_matches("*.env", ".env.local"));
        assert!(wildcard_matches("*.env.*", ".env.local"));
        assert!(wildcard_matches("git status *", "git status --short"));
        assert!(wildcard_matches("?.rs", "a.rs"));
        assert!(!wildcard_matches("?.rs", "ab.rs"));
    }

    #[test]
    fn agent_file_injects_every_skill_and_states_nothing_it_cannot_enforce() {
        let skills = vec![
            Skill {
                name: "break-repetition-loop".into(),
                body: "When a command fails twice identically, change approach.\n".into(),
                purpose: None,
            },
            Skill {
                name: "read-before-edit".into(),
                body: "Read a file before editing it.".into(),
                purpose: None,
            },
        ];
        let text = render_agent_file("wiki-inference", &skills);
        assert!(text.starts_with("---\n"));
        assert!(text.contains("## break-repetition-loop"));
        assert!(text.contains("## read-before-edit"));
        assert!(text.contains("change approach."));
        // The rollout can write this file: a permission stated here would be advice, and
        // stating it would suggest the boundary lives somewhere it does not.
        assert!(!text.contains("permission"), "{text}");
    }

    #[test]
    fn agent_file_is_byte_identical_for_the_same_skills() {
        let skills = vec![Skill {
            name: "s".into(),
            body: "do the thing".into(),
            purpose: None,
        }];
        assert_eq!(
            render_agent_file("wiki-inference", &skills),
            render_agent_file("wiki-inference", &skills)
        );
    }

    #[test]
    fn the_empty_skill_baseline_still_renders_a_usable_agent() {
        let text = render_agent_file("wiki-inference", &[]);
        assert!(text.contains("no additional skills"));
        assert!(!text.contains("# Skills"));
    }
}
