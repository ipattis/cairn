// The cockpit's whole job is to render what the daemon says and send back what the user
// clicks. There is no loop logic here on purpose: a run must survive this window closing.
//
// Everything model-written (skill bodies, diffs, proposal summaries, the impact log) is put
// into the DOM with textContent, never innerHTML — a skill is text a model wrote, and this
// window has IPC access to the daemon.

const invoke = window.__TAURI__.core.invoke;

const REFRESH_MS = 4000;
let view = "runs";
let openRun = null;
let schedules = [];
let timer = null;

async function api(method, path, body) {
  const reply = await invoke("api", { method, path, body: body ?? null });
  if (reply.status >= 400) {
    let detail = reply.text;
    try {
      detail = JSON.parse(reply.text).error ?? detail;
    } catch (_) {
      /* not JSON: show it raw */
    }
    throw new Error(detail);
  }
  return reply.text;
}

async function json(method, path, body) {
  const text = await api(method, path, body);
  return text.length ? JSON.parse(text) : null;
}

function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key === "on") for (const [ev, fn] of Object.entries(value)) node.addEventListener(ev, fn);
    else node.setAttribute(key, value);
  }
  for (const child of [].concat(children)) {
    if (child) node.appendChild(child);
  }
  return node;
}

function chip(text, kind) {
  return el("span", { class: kind ? `chip ${kind}` : "chip", text });
}

function pct(value) {
  return value === null || value === undefined ? "—" : `${(value * 100).toFixed(1)}%`;
}

function obsidian(link, label) {
  return el("a", {
    text: label,
    on: { click: () => invoke("open_obsidian", { link }).catch(showError) },
  });
}

function showError(error) {
  const right = document.getElementById("footer-right");
  right.className = "err";
  right.textContent = String(error.message ?? error);
}

function clearError() {
  const right = document.getElementById("footer-right");
  right.className = "";
  right.textContent = "";
}

function replace(id, nodes) {
  const host = document.getElementById(id);
  host.textContent = "";
  for (const node of [].concat(nodes)) if (node) host.appendChild(node);
}

// ---------------------------------------------------------------- status

async function refreshStatus() {
  const chipNode = document.getElementById("status-chip");
  try {
    const status = await json("GET", "/v1/status");
    clearError();
    const parts = [
      status.active_run ? `run ${status.active_run}` : "idle",
      `${status.active_skills.length} skill(s)`,
      status.executor_running ? "executor up" : "executor down",
    ];
    chipNode.className = status.active_run ? "chip warn" : "chip ok";
    chipNode.textContent = parts.join(" · ");
    document.getElementById("footer-left").textContent =
      `${status.vault} · gate: ${status.gate_rule} · jev: ${
        status.jev_enabled ? (status.jev_shadow_mode ? "shadow" : "live") : "off"
      } · executor pinned ${status.executor_pinned}`;
    const plan = document.getElementById("run-plan");
    plan.textContent = status.task_set_error
      ? `task set not ready: ${status.task_set_error}`
      : `${status.planned_rollouts} rollouts planned for a full run`;
    plan.className = status.task_set_error ? "hint err" : "hint";
    document.getElementById("start-run").disabled = !!status.active_run || !!status.task_set_error;
    document.getElementById("run-baseline").disabled = !!status.active_run || !!status.task_set_error;
  } catch (error) {
    chipNode.className = "chip bad";
    chipNode.textContent = "daemon unreachable";
    showError(error);
  }
}

// ---------------------------------------------------------------- runs

let lastRunsJson = null;

async function refreshRuns(force = false) {
  const text = await api("GET", "/v1/runs");
  // Re-rendering identical data on every tick would close a diff someone is reading.
  if (!force && text === lastRunsJson) return;
  lastRunsJson = text;
  const runs = JSON.parse(text);
  replace(
    "runs-list",
    runs.length
      ? runs.map((run) => runCard(run))
      : [el("p", { class: "hint", text: "No runs yet. `Measure empty-skill baseline` first: it is the bar every proposal has to beat." })],
  );
  if (openRun) {
    const run = runs.find((r) => r.id === openRun);
    replace("run-detail", run ? runDetail(run) : []);
  } else {
    replace("run-detail", []);
  }
}

function runCard(run) {
  const active = !["completed", "completed_perfect", "failed", "cancelled"].includes(run.status);
  const controls = [];
  if (active) {
    controls.push(
      el("button", { text: run.status === "paused" ? "Resume" : "Pause", on: { click: () => act(run, run.status === "paused" ? "resume" : "pause") } }),
      el("button", { text: "Cancel", on: { click: () => act(run, "cancel") } }),
    );
  }
  return el("div", { class: "card" }, [
    el("div", { class: "row" }, [
      el("h3", { text: run.id }),
      chip(run.status.replace("_", " "), run.status === "failed" ? "bad" : active ? "warn" : "ok"),
      chip(`best ${pct(run.best)}`),
      chip(`baseline ${pct(run.baseline_val)}`),
      run.test_score !== null && run.test_score !== undefined ? chip(`test ${pct(run.test_score)}`) : null,
      chip(`${run.iterations.length}/${run.iterations_planned} iterations`),
      el("span", { class: "hint", text: run.phase }),
      el("button", {
        text: openRun === run.id ? "Hide" : "Details",
        on: {
          click: () => {
            openRun = openRun === run.id ? null : run.id;
            refresh();
          },
        },
      }),
      ...controls,
    ]),
    run.error ? el("p", { class: "err", text: run.error }) : null,
  ]);
}

async function act(run, what) {
  try {
    const ack = await json("POST", `/v1/runs/${encodeURIComponent(run.id)}/${what}`);
    clearError();
    document.getElementById("footer-right").textContent = ack.detail;
  } catch (error) {
    showError(error);
  }
  refresh();
}

function runDetail(run) {
  const rows = run.iterations.map((it) => {
    const decision = describeDecision(it.decision);
    return el("tr", {}, [
      el("td", { text: String(it.iteration) }),
      el("td", { text: it.skill ?? "—" }),
      el("td", { text: pct(it.train_score) }),
      el("td", { text: it.val_score === null ? "not scored" : pct(it.val_score) }),
      el("td", { text: pct(it.best) }),
      el("td", { text: decision }),
      el("td", { text: it.repeat ? (it.repeat.repeat ? "repeat" : "novel") : "—" }),
      el("td", {}, [
        it.diff
          ? el("button", {
              text: "diff",
              on: {
                click: (event) => {
                  event.target.closest("tr").nextElementSibling.classList.toggle("hidden");
                },
              },
            })
          : null,
      ]),
    ]);
  });

  const body = [];
  run.iterations.forEach((it, index) => {
    body.push(rows[index]);
    body.push(
      el("tr", { class: "hidden" }, [
        el("td", { colspan: "8" }, [
          el("pre", { class: "diff", text: it.diff || "(no diff recorded)" }),
          ...(it.raw_notes ?? []).map(noteBlock),
        ]),
      ]),
    );
  });

  return el("div", { class: "card" }, [
    el("h3", { text: `${run.id} · ${run.model_id}` }),
    el("div", { class: "row" }, [
      chip(`gate ${run.gate_rule}`),
      chip(`inference ${tokens(run.usage.inference)}`),
      chip(`maintainer ${tokens(run.usage.maintainer)}`),
      chip(`proposer ${tokens(run.usage.proposer)}`),
    ]),
    el("table", {}, [
      el("thead", {}, [
        el("tr", {}, ["iter", "skill", "train", "val", "best", "decision", "repeat", ""].map((h) => el("th", { text: h }))),
      ]),
      el("tbody", {}, body),
    ]),
  ]);
}

// `Decision` serialises as "accepted", or {"rejected": reason} where reason is
// "not_better", "no_change", or {"diff_scope": {"folders": [...]}}.
function describeDecision(decision) {
  if (decision === "accepted") return "accepted";
  const reason = decision?.rejected;
  if (reason === "not_better") return "rejected: did not strictly beat best";
  if (reason === "no_change") return "rejected: changed nothing";
  if (reason?.diff_scope) {
    return `rejected unscored: touched ${reason.diff_scope.folders.join(", ")}`;
  }
  return `rejected: ${JSON.stringify(reason ?? decision)}`;
}

function tokens(usage) {
  if (!usage) return "0 tok";
  const total = (usage.input_tokens ?? 0) + (usage.output_tokens ?? 0);
  const cached = usage.cached_input_tokens ?? 0;
  return cached ? `${total} tok (${cached} cached)` : `${total} tok`;
}

/// One raw trace, read through the daemon. Raw notes are read-only in the vault, so this is
/// a viewer and never an editor.
function noteBlock(path) {
  const pre = el("pre", { class: "diff hidden" });
  return el("div", {}, [
    el("a", {
      text: path,
      on: {
        click: async () => {
          pre.classList.remove("hidden");
          try {
            pre.textContent = await api("GET", `/v1/note?path=${encodeURIComponent(path)}`);
          } catch (error) {
            pre.textContent = String(error.message);
          }
        },
      },
    }),
    pre,
  ]);
}

// ---------------------------------------------------------------- other views

async function refreshGate() {
  try {
    document.getElementById("impact").textContent = await api("GET", "/v1/impact");
  } catch (error) {
    document.getElementById("impact").textContent = `no impact log yet: ${error.message}`;
  }
}

async function refreshSkills() {
  const skills = await json("GET", "/v1/skills");
  replace(
    "skills-list",
    skills.length
      ? skills.map((skill) =>
          el("div", { class: "card" }, [
            el("div", { class: "row" }, [
              el("h3", { text: skill.name }),
              obsidian(skill.obsidian_link, "open in Obsidian"),
              el("button", { text: "history", on: { click: (e) => showHistory(skill.name, e.target) } }),
            ]),
            skill.purpose ? el("pre", { class: "diff", text: skill.purpose }) : null,
            el("pre", { class: "doc", text: skill.body }),
          ]),
        )
      : [el("p", { class: "hint", text: "No skills yet — this is the empty-skill state the baseline measures." })],
  );
}

async function showHistory(name, button) {
  try {
    const history = await json("GET", `/v1/skills/${encodeURIComponent(name)}/history`);
    const host = el("div", {}, history.map((entry) =>
      el("div", { class: "row" }, [
        el("span", { class: "hint", text: entry.commit.slice(0, 12) }),
        el("span", { text: entry.subject }),
        el("button", {
          text: "revert to this",
          on: {
            click: async () => {
              try {
                const ack = await json("POST", `/v1/skills/${encodeURIComponent(name)}/revert`, { commit: entry.commit });
                document.getElementById("footer-right").textContent = ack.detail;
                refresh();
              } catch (error) {
                showError(error);
              }
            },
          },
        }),
      ]),
    ));
    button.closest(".card").appendChild(host);
    button.disabled = true;
  } catch (error) {
    showError(error);
  }
}

async function refreshPatterns() {
  const patterns = await json("GET", "/v1/patterns");
  replace(
    "patterns-list",
    patterns.length
      ? patterns.map((p) =>
          el("div", { class: "card" }, [
            el("div", { class: "row" }, [
              el("h3", { text: p.name }),
              chip(`${p.occurrences} occurrence(s)`),
              obsidian(p.obsidian_link, "open in Obsidian"),
            ]),
          ]),
        )
      : [el("p", { class: "hint", text: "No patterns recorded yet." })],
  );
}

async function refreshLabels() {
  const labels = await json("GET", "/v1/labels");
  const open = labels.filter((l) => l.human_verdict === null || l.human_verdict === undefined);
  replace(
    "labels-list",
    open.length
      ? open.map((label) =>
          el("div", { class: "card" }, [
            el("div", { class: "row" }, [
              el("h3", { text: label.task_id }),
              chip(`iter ${label.iteration}`),
              chip(label.confidence === null ? "no confidence" : `confidence ${label.confidence.toFixed(2)}`),
              label.suggested_failure_type ? chip(label.suggested_failure_type) : null,
              el("button", { text: "read the trace", on: { click: () => showNote(label.note_path) } }),
              el("button", { text: "passed", on: { click: () => resolve(label, true) } }),
              el("button", { text: "failed", on: { click: () => resolve(label, false) } }),
            ]),
            el("pre", { class: "diff hidden", id: `note-${cssId(label.note_path)}` }),
          ]),
        )
      : [el("p", { class: "hint", text: "Nothing awaiting a human verdict." })],
  );
}

function cssId(path) {
  return path.replace(/[^a-zA-Z0-9_-]/g, "-");
}

async function showNote(path) {
  const host = document.getElementById(`note-${cssId(path)}`);
  host.classList.remove("hidden");
  try {
    host.textContent = await api("GET", `/v1/note?path=${encodeURIComponent(path)}`);
  } catch (error) {
    host.textContent = String(error.message);
  }
}

async function resolve(label, verdict) {
  try {
    await json("POST", "/v1/labels", { note_path: label.note_path, verdict });
    refresh();
  } catch (error) {
    showError(error);
  }
}

async function refreshSchedule() {
  schedules = await json("GET", "/v1/schedules");
  replace(
    "schedule-list",
    schedules.map((schedule, index) => {
      const enabled = el("input", { type: "checkbox" });
      enabled.checked = schedule.enabled;
      enabled.addEventListener("change", () => (schedules[index].enabled = enabled.checked));
      const hour = el("input", { type: "number", min: "0", max: "23", size: "3", value: String(schedule.hour) });
      hour.addEventListener("change", () => (schedules[index].hour = Number(hour.value)));
      return el("div", { class: "card" }, [
        el("div", { class: "row" }, [
          enabled,
          el("h3", { text: schedule.id }),
          el("span", { class: "hint", text: "hour" }),
          hour,
          schedule.weekday !== null && schedule.weekday !== undefined
            ? chip(["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][schedule.weekday])
            : chip("daily"),
          schedule.last_run ? chip(`last ran ${new Date(schedule.last_run).toLocaleString()}`) : chip("never run"),
        ]),
      ]);
    }),
  );

  const proposals = await json("GET", "/v1/proposals");
  const open = proposals.filter((p) => p.accepted === null || p.accepted === undefined);
  replace(
    "proposals-list",
    open.length
      ? open.map((proposal) =>
          el("div", { class: "card" }, [
            el("div", { class: "row" }, [
              el("h3", { text: proposal.skill ?? proposal.id }),
              chip(proposal.source),
              chip(new Date(proposal.created).toLocaleString()),
              el("button", { class: "primary", text: "accept", on: { click: () => reviewProposal(proposal.id, "accept") } }),
              el("button", { text: "reject", on: { click: () => reviewProposal(proposal.id, "reject") } }),
            ]),
            el("p", { class: "hint", text: proposal.summary }),
            el("pre", { class: "diff", text: proposal.diff }),
          ]),
        )
      : [el("p", { class: "hint", text: "No proposals awaiting review." })],
  );
}

async function reviewProposal(id, what) {
  try {
    const ack = await json("POST", `/v1/proposals/${encodeURIComponent(id)}/${what}`);
    document.getElementById("footer-right").textContent = ack.detail;
    refresh();
  } catch (error) {
    showError(error);
  }
}

async function saveSchedules() {
  try {
    const ack = await json("PUT", "/v1/schedules", schedules);
    document.getElementById("footer-right").textContent = ack.detail;
  } catch (error) {
    showError(error);
  }
}

// ---------------------------------------------------------------- setup: obsidian

// Rendered in the banner and in the Setup tab from the same daemon response, so the two can
// never disagree about whether the vault is registered.
let obsidianState = null;

async function refreshObsidian() {
  obsidianState = await json("GET", "/v1/obsidian");
  renderObsidianBanner();

  const rows = [
    el("div", { class: "card" }, [
      el("div", { class: "row" }, [
        el("h3", { text: obsidianState.vault_name || "vault" }),
        chip(obsidianState.installed ? "Obsidian installed" : "Obsidian not found", obsidianState.installed ? "ok" : "bad"),
        chip(obsidianState.registered ? "vault registered" : "vault not registered", obsidianState.registered ? "ok" : "warn"),
        chip(obsidianState.prepared ? "settings written" : "settings not written", obsidianState.prepared ? "ok" : "warn"),
      ]),
      el("p", { class: "path", text: obsidianState.vault_path }),
      obsidianState.registered
        ? obsidian(obsidianState.open_link, "open the wiki in Obsidian")
        : null,
    ]),
  ];

  if (obsidianState.steps.length) {
    rows.push(
      el("div", { class: "card" }, [
        el("h3", { text: "To finish, in Obsidian itself" }),
        el(
          "ol",
          {},
          obsidianState.steps.map((step) => el("li", { text: step })),
        ),
      ]),
    );
  }
  replace("obsidian-state", rows);
}

function renderObsidianBanner() {
  const banner = document.getElementById("setup-banner");
  // Nothing to say once the vault is registered, and a permanent banner trains people to
  // ignore banners.
  if (!obsidianState || obsidianState.registered) {
    banner.classList.add("hidden");
    banner.textContent = "";
    return;
  }
  banner.classList.remove("hidden");
  replace("setup-banner", [
    el("span", {
      text: obsidianState.installed
        ? "Obsidian does not know this vault yet, so note links here will not open."
        : "Obsidian is not installed, so note links here will not open.",
    }),
    el("button", { text: "Set it up", on: { click: () => switchTo("setup") } }),
  ]);
}

async function prepareObsidian() {
  try {
    const ack = await json("POST", "/v1/obsidian/prepare");
    document.getElementById("footer-right").textContent = ack.detail;
    await refreshObsidian();
  } catch (error) {
    showError(error);
  }
}

// ---------------------------------------------------------------- setup: credentials

async function refreshCredentials() {
  const credentials = await json("GET", "/v1/credentials");
  replace(
    "credentials-list",
    credentials.map((credential) => {
      const input = el("input", {
        type: "password",
        placeholder: credential.in_keychain ? "replace the stored value" : "paste the key",
        size: "44",
        autocomplete: "off",
        spellcheck: "false",
      });
      const save = el("button", {
        class: "primary",
        text: "Save",
        on: { click: () => saveCredential(credential, input) },
      });
      // Enter is what anyone pasting into a single field will press.
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter") saveCredential(credential, input);
      });

      return el("div", { class: "card" }, [
        el("div", { class: "row" }, [
          el("h3", { text: credential.label }),
          chip(credential.in_keychain ? "in Keychain" : "not set", credential.in_keychain ? "ok" : credential.required ? "bad" : "warn"),
          credential.in_daemon_env ? chip("loaded in daemon", "ok") : null,
          chip(credential.required ? "required" : "optional for this config"),
        ]),
        el("p", { class: "hint", text: credential.purpose }),
        el("p", { class: "path", text: `${credential.service} → ${credential.env}` }),
        el("div", { class: "row" }, [
          input,
          save,
          credential.in_keychain
            ? el("button", { text: "Remove", on: { click: () => removeCredential(credential) } })
            : null,
        ]),
      ]);
    }),
  );
}

async function saveCredential(credential, input) {
  const value = input.value;
  if (!value) {
    showError(new Error(`nothing to save for ${credential.label}`));
    return;
  }
  try {
    const ack = await json("PUT", `/v1/credentials/${encodeURIComponent(credential.service)}`, { value });
    document.getElementById("footer-right").textContent = ack.detail;
    clearError();
  } catch (error) {
    showError(error);
  } finally {
    // Cleared whether or not the save worked: a retry should re-paste rather than leave a key
    // sitting in a DOM node.
    input.value = "";
  }
  await refreshCredentials();
  await refreshStatus();
}

async function removeCredential(credential) {
  try {
    const ack = await json("DELETE", `/v1/credentials/${encodeURIComponent(credential.service)}`);
    document.getElementById("footer-right").textContent = ack.detail;
  } catch (error) {
    showError(error);
  }
  await refreshCredentials();
  await refreshStatus();
}

async function refreshSetup() {
  await refreshObsidian();
  await refreshCredentials();
}

// ---------------------------------------------------------------- wiring

const REFRESHERS = {
  // An explicit refresh always re-renders: the user just clicked something.
  runs: () => refreshRuns(true),
  gate: refreshGate,
  skills: refreshSkills,
  patterns: refreshPatterns,
  labels: refreshLabels,
  schedule: refreshSchedule,
  setup: refreshSetup,
};

async function refresh() {
  await refreshStatus();
  try {
    await REFRESHERS[view]();
  } catch (error) {
    showError(error);
  }
}

/// The timer only re-renders the Runs view. Re-rendering the others every few seconds would
/// collapse an expanded diff or trace out from under whoever is reading it.
async function tick() {
  await refreshStatus();
  if (view === "runs") {
    try {
      await refreshRuns();
    } catch (error) {
      showError(error);
    }
  }
}

function switchTo(next) {
  view = next;
  for (const button of document.querySelectorAll("#tabs button")) {
    button.classList.toggle("active", button.dataset.view === next);
  }
  for (const section of document.querySelectorAll(".view")) {
    section.classList.toggle("hidden", section.id !== `view-${next}`);
  }
  refresh();
}

document.querySelectorAll("#tabs button").forEach((button) => {
  button.addEventListener("click", () => switchTo(button.dataset.view));
});

document.getElementById("start-run").addEventListener("click", async () => {
  const model = document.getElementById("model-override").value.trim();
  try {
    const created = await json("POST", "/v1/runs", model ? { model } : {});
    openRun = created.id;
    refresh();
  } catch (error) {
    showError(error);
  }
});

document.getElementById("run-baseline").addEventListener("click", async (event) => {
  event.target.disabled = true;
  event.target.textContent = "measuring…";
  try {
    const run = await json("POST", "/v1/baseline");
    document.getElementById("footer-right").textContent =
      `baseline: validation ${pct(run.baseline_val)}${
        run.baseline_test === null ? "" : `, test ${pct(run.baseline_test)}`
      }`;
  } catch (error) {
    showError(error);
  } finally {
    event.target.textContent = "Measure empty-skill baseline";
    refresh();
  }
});

document.getElementById("save-schedules").addEventListener("click", saveSchedules);

document.getElementById("obsidian-recheck").addEventListener("click", () => {
  refreshObsidian().catch(showError);
});
document.getElementById("obsidian-prepare").addEventListener("click", prepareObsidian);
document.getElementById("obsidian-open-app").addEventListener("click", () => {
  invoke("launch_obsidian").catch(showError);
});
document.getElementById("obsidian-reveal").addEventListener("click", () => {
  invoke("reveal_vault")
    .then((path) => (document.getElementById("footer-right").textContent = `revealed ${path}`))
    .catch(showError);
});

invoke("where_is_the_daemon").then((base) => {
  document.getElementById("footer-left").textContent = base || "daemon not configured";
});

// Checked once at startup, whichever tab is open: the banner is the thing that sends a new
// user to Setup before they wonder why a note link did nothing.
refreshObsidian().catch(() => {
  /* the footer already shows why the daemon is unreachable */
});

refresh();
timer = setInterval(tick, REFRESH_MS);
window.addEventListener("beforeunload", () => clearInterval(timer));
