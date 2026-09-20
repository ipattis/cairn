/**
 * Captures finished OpenCode sessions into the WikiSkill vault.
 *
 * Phase 4 of the rationale: the user's real coding sessions are traces like any other, so
 * they should feed the same wiki. This plugin does no interpreting — it reads the session
 * through the plugin context and posts it to the daemon, which redacts it, labels it and files
 * it. Redaction happens *there*, not here, because the daemon is the only thing that must be
 * trusted with that; a plugin that redacted locally would be one more place to get it wrong.
 *
 * Deliberately limited to the plugin context and `fetch`: no shell, no filesystem, no
 * dependencies. A plugin runs inside the user's daily agent, which is the last place that
 * should be able to run commands or read keys.
 *
 * This is the **OpenCode V2** plugin API. V1 implementations do not run in V2 at all, so there
 * is no compatibility shim here: the harness moved to V2 wholesale, and a file that loaded
 * under both would only make it harder to tell which one is running.
 *
 * Install (V2 discovers `plugins/`, plural, under its config directory):
 *   mkdir -p ~/.config/opencode/plugins
 *   ln -s "$PWD/plugins/opencode-capture/index.js" \
 *     ~/.config/opencode/plugins/wikiskill-capture.js
 *
 * Configure (the token is the daemon's; `wikiskilld token` prints it):
 *   export WIKISKILL_API="http://127.0.0.1:8787"
 *   export WIKISKILL_API_TOKEN="$(wikiskilld token)"
 *
 * Without the token the plugin stays silent rather than failing sessions: capture is a
 * convenience, and a daily coding session must never break because the daemon is down.
 */

import { Plugin } from "@opencode/plugin";

const DEFAULT_BASE = "http://127.0.0.1:8787";

/** Joins the text of a V2 tool-content list, naming files it cannot inline. */
function renderToolContent(content) {
  return (content ?? [])
    .map((part) =>
      part?.type === "text" ? (part.text ?? "") : part?.uri ? `[file ${part.uri}]` : "",
    )
    .filter(Boolean)
    .join("\n");
}

/**
 * Maps one `Session.Message.Info` to the daemon's transcript shape.
 *
 * V2 flattened V1's `{info, parts}` envelope: a message carries its own `content` array, the
 * model is a `{providerID, id}` ref rather than a bare string, and a tool part's result lives
 * in `state.content` as parts instead of `state.output` as a string. Returns `null` for the
 * message kinds that are bookkeeping rather than transcript — `idle` above all, which is the
 * entry that told us the session was worth capturing in the first place.
 */
function toMessage(entry) {
  if (entry?.type === "user") {
    return { role: "user", text: entry.text ?? "", tool_calls: [] };
  }
  if (entry?.type !== "assistant") return null;

  const message = { role: "assistant", text: "", tool_calls: [] };
  if (entry.model?.id) message.model = entry.model.id;
  if (entry.tokens) {
    message.tokens = {
      input_tokens: entry.tokens.input ?? 0,
      output_tokens: entry.tokens.output ?? 0,
      cached_input_tokens: entry.tokens.cache?.read ?? 0,
    };
  }

  const text = [];
  const reasoning = [];
  for (const part of entry.content ?? []) {
    if (part.type === "text" && part.text) text.push(part.text);
    else if (part.type === "reasoning" && part.text) reasoning.push(part.text);
    else if (part.type === "tool") {
      const state = part.state ?? {};
      message.tool_calls.push({
        name: part.name ?? "unknown",
        input: state.input ?? null,
        output: state.error ?? renderToolContent(state.content),
        // A tool that errored is the single most useful thing in a trace, so the daemon is
        // told explicitly rather than having to infer it from the output text. A tool still
        // streaming or running is not a success either: only `completed` is.
        ok: state.status === "completed",
      });
    }
  }
  message.text = text.join("\n");
  if (reasoning.length) message.reasoning = reasoning.join("\n");
  return message;
}

/** Posts one session to the daemon. Returns quietly on anything short of success. */
async function capture(ctx, sessionID, base, token) {
  const messages = (await ctx.session.context({ sessionID })).map(toMessage).filter(Boolean);
  if (!messages.length) return false;

  let title;
  try {
    title = (await ctx.session.get({ sessionID }))?.title;
  } catch {
    // A missing title is cosmetic; the session id is the fallback in the daemon.
  }

  const response = await fetch(`${base}/v1/capture`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${token}` },
    body: JSON.stringify({
      session: sessionID,
      title,
      directory: ctx.location.directory,
      messages,
    }),
  });

  if (!response.ok) {
    console.error(`wikiskill: capture refused (${response.status}): ${await response.text()}`);
    return false;
  }
  const body = await response.json();
  console.log(`wikiskill: captured to ${body.note} (${body.redactions})`);
  return true;
}

export default Plugin.define({
  id: "wikiskill.capture",

  setup(ctx) {
    const base = process.env.WIKISKILL_API ?? DEFAULT_BASE;
    const token = process.env.WIKISKILL_API_TOKEN;
    // Sessions already sent. `session.idle` can fire more than once for a session; the daemon
    // would happily file it twice.
    const sent = new Set();
    // V1 returned an `event` hook and let the runtime drive it. V2 hands out an async iterator
    // instead, so the plugin owns the loop — and therefore owns stopping it. Without the abort
    // in the cleanup function, a reload would leave the old subscription capturing alongside
    // the new one, and every session would be filed twice.
    const controller = new AbortController();

    void (async () => {
      try {
        for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
          if (event.type !== "session.idle") continue;
          if (!token) continue;

          const sessionID = event.properties?.sessionID;
          if (!sessionID || sent.has(sessionID)) continue;
          // Reserved before the await, not after: a second `session.idle` can arrive while the
          // first capture is still in flight.
          sent.add(sessionID);
          try {
            if (!(await capture(ctx, sessionID, base, token))) sent.delete(sessionID);
          } catch (error) {
            // Never let capture break a session the user is in the middle of.
            sent.delete(sessionID);
            console.error(`wikiskill: capture failed: ${error}`);
          }
        }
      } catch (error) {
        // Aborting the signal is how the subscription is *meant* to end, and it ends by
        // throwing. Swallowing it here is what keeps a reload from logging an unhandled
        // rejection every time.
        if (!controller.signal.aborted) {
          console.error(`wikiskill: event subscription ended: ${error}`);
        }
      }
    })();

    return () => controller.abort();
  },
});
