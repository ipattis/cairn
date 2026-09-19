/**
 * Captures finished OpenCode sessions into the WikiSkill vault.
 *
 * Phase 4 of the rationale: the user's real coding sessions are traces like any other, so
 * they should feed the same wiki. This plugin does no interpreting — it reads the session
 * with the SDK client and posts it to the daemon, which redacts it, labels it and files it.
 * Redaction happens *there*, not here, because the daemon is the only thing that must be
 * trusted with that; a plugin that redacted locally would be one more place to get it wrong.
 *
 * Deliberately limited to the SDK `client` and `fetch`: no shell, no filesystem, no
 * dependencies. A plugin runs inside the user's daily agent, which is the last place that
 * should be able to run commands or read keys.
 *
 * Install:
 *   ln -s "$PWD/plugins/opencode-capture/index.js" \
 *     ~/.config/opencode/plugin/wikiskill-capture.js
 *
 * Configure (the token is the daemon's; `wikiskilld token` prints it):
 *   export WIKISKILL_API="http://127.0.0.1:8787"
 *   export WIKISKILL_API_TOKEN="$(wikiskilld token)"
 *
 * Without the token the plugin stays silent rather than failing sessions: capture is a
 * convenience, and a daily coding session must never break because the daemon is down.
 */

const DEFAULT_BASE = "http://127.0.0.1:8787";

/** Maps one SDK message envelope to the daemon's transcript shape. */
function toMessage(envelope) {
  const info = envelope.info ?? {};
  const message = {
    role: info.role ?? "unknown",
    text: "",
    tool_calls: [],
  };
  if (info.modelID) message.model = info.modelID;
  if (info.tokens) {
    message.tokens = {
      input_tokens: info.tokens.input ?? 0,
      output_tokens: info.tokens.output ?? 0,
      cached_input_tokens: info.tokens.cache?.read ?? 0,
    };
  }

  const text = [];
  const reasoning = [];
  for (const part of envelope.parts ?? []) {
    if (part.type === "text" && part.text) text.push(part.text);
    else if (part.type === "reasoning" && part.text) reasoning.push(part.text);
    else if (part.type === "tool") {
      const state = part.state ?? {};
      message.tool_calls.push({
        name: part.tool ?? "unknown",
        input: state.input ?? null,
        output: state.error ?? state.output ?? "",
        // A tool that errored is the single most useful thing in a trace, so the daemon is
        // told explicitly rather than having to infer it from the output text.
        ok: !state.error && (state.status === "completed" || state.status === undefined),
      });
    }
  }
  message.text = text.join("\n");
  if (reasoning.length) message.reasoning = reasoning.join("\n");
  return message;
}

export const WikiSkillCapture = async ({ client, directory }) => {
  const base = process.env.WIKISKILL_API ?? DEFAULT_BASE;
  const token = process.env.WIKISKILL_API_TOKEN;
  // Sessions the plugin has already sent. `session.idle` can fire more than once for a
  // session; the daemon would happily file it twice.
  const sent = new Set();

  return {
    event: async ({ event }) => {
      if (event.type !== "session.idle") return;
      if (!token) return;

      const sessionId = event.properties?.sessionID ?? event.properties?.sessionId;
      if (!sessionId || sent.has(sessionId)) return;

      try {
        const { data: messages } = await client.session.messages({
          path: { id: sessionId },
        });
        if (!messages?.length) return;

        let title;
        try {
          const { data: session } = await client.session.get({ path: { id: sessionId } });
          title = session?.title;
        } catch {
          // A missing title is cosmetic; the session id is the fallback in the daemon.
        }

        const response = await fetch(`${base}/v1/capture`, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            authorization: `Bearer ${token}`,
          },
          body: JSON.stringify({
            session: sessionId,
            title,
            directory,
            messages: messages.map(toMessage),
          }),
        });

        if (!response.ok) {
          console.error(
            `wikiskill: capture refused (${response.status}): ${await response.text()}`,
          );
          return;
        }
        sent.add(sessionId);
        const body = await response.json();
        console.log(`wikiskill: captured to ${body.note} (${body.redactions})`);
      } catch (error) {
        // Never let capture break a session the user is in the middle of.
        console.error(`wikiskill: capture failed: ${error}`);
      }
    },
  };
};

export default WikiSkillCapture;
